#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use portagenty::domain::Session;
use portagenty::mux::{Multiplexer, TmuxAdapter, ZellijAdapter};
use portagenty::supervision::{
    BindingReceipt, CapabilityState, LinuxSystemdBackend, LogicalSessionId, MetricValue, MuxTarget,
    OwnershipState, ReceiptStore, SoftLimits, SupervisionBackend,
};
use uuid::Uuid;

const TEST_COMMAND: &str = "sleep 300";

struct LiveCleanup {
    receipt: Option<BindingReceipt>,
    store: ReceiptStore,
}

impl LiveCleanup {
    fn new(store: ReceiptStore) -> Self {
        Self {
            receipt: None,
            store,
        }
    }

    fn arm(&mut self, receipt: BindingReceipt) {
        self.receipt = Some(receipt);
    }

    fn disarm(&mut self) {
        self.receipt = None;
    }
}

impl Drop for LiveCleanup {
    fn drop(&mut self) {
        let Some(receipt) = self.receipt.as_ref() else {
            return;
        };
        let _ = graceful_stop_target(receipt);
        if let Ok(backend) = LinuxSystemdBackend::connect() {
            let _ = backend.stop_unit(receipt);
        }
        let _ = self.store.remove(&receipt.logical_id);
    }
}

#[test]
#[ignore = "creates a synthetic transient systemd user service and private tmux server"]
fn supervised_tmux_create_snapshot_and_graceful_stop() {
    run_live_test(MuxKind::Tmux).unwrap();
}

#[test]
#[ignore = "creates a synthetic transient systemd user service and private Zellij server"]
fn supervised_zellij_create_snapshot_and_graceful_stop() {
    run_live_test(MuxKind::Zellij).unwrap();
}

#[derive(Debug, Clone, Copy)]
enum MuxKind {
    Tmux,
    Zellij,
}

fn run_live_test(kind: MuxKind) -> Result<()> {
    let backend = LinuxSystemdBackend::connect().context("connecting to systemd user manager")?;
    let capability = backend.capabilities();
    if capability.overall != CapabilityState::Supported {
        bail!(
            "systemd supervision capability is unavailable: {:?}; notes: {:?}",
            capability.overall,
            capability.notes
        );
    }

    let temp = assert_fs::TempDir::new_in("/tmp").context("creating isolated test directory")?;
    let workdir = temp.path().join("workdir");
    std::fs::create_dir(&workdir).context("creating synthetic session working directory")?;
    let store = ReceiptStore::new(temp.path().join("supervision.toml"));
    let mut cleanup = LiveCleanup::new(store.clone());

    let logical_id = LogicalSessionId::new(
        Uuid::new_v4().to_string(),
        match kind {
            MuxKind::Tmux => "systemd-e2e-tmux",
            MuxKind::Zellij => "systemd-e2e-zellij",
        },
    )?;
    let session = Session {
        name: logical_id.session_name.clone(),
        cwd: workdir,
        command: TEST_COMMAND.into(),
        kind: None,
        env: BTreeMap::new(),
        description: Some("synthetic supervision integration test".into()),
    };
    let limits = SoftLimits {
        memory_high_bytes: Some(512 * 1024 * 1024),
        cpu_quota_percent: Some(100.0),
        tasks_max: Some(256),
    };

    let receipt = match kind {
        MuxKind::Tmux => {
            backend.create_tmux_binding(&store, logical_id.clone(), &session, limits.clone())?
        }
        MuxKind::Zellij => backend.create_zellij_binding(
            &store,
            logical_id.clone(),
            "systemd-e2e",
            &session,
            limits.clone(),
        )?,
    };
    cleanup.arm(receipt.clone());

    if receipt.logical_id != logical_id {
        bail!("receipt logical identity does not match the synthetic session");
    }
    if receipt.limits != limits {
        bail!("receipt does not preserve the requested soft guardrails");
    }
    if store.find(&logical_id)?.as_ref() != Some(&receipt) {
        bail!("temporary receipt store does not contain the created binding");
    }
    if !matches!(
        backend.reconcile(&receipt)?,
        OwnershipState::OwnedVerified(_)
    ) {
        bail!("new binding did not reconcile as owned and verified");
    }

    let first = backend.snapshot(&receipt, None)?;
    require_value("memory.current", &first.memory_current_bytes)?;
    require_value("pids.current", &first.tasks_current)?;
    require_value("cgroup.events", &first.cgroup_state)?;

    thread::sleep(Duration::from_millis(150));
    let second = backend.snapshot(&receipt, Some(&first))?;
    require_value("cpu sampled rate", &second.cpu_percent)?;
    require_value("I/O sampled read rate", &second.io_read_bytes_per_sec)?;
    require_value("I/O sampled write rate", &second.io_write_bytes_per_sec)?;

    graceful_stop_target(&receipt)?;
    require_target_absent(&receipt)?;
    let stopped = backend.stop_unit(&receipt)?;
    if !stopped.completed {
        bail!("non-force systemd stop did not complete: {stopped:?}");
    }
    let inactive = backend.stop_unit(&receipt)?;
    if !inactive.completed || !inactive.attempted.is_empty() {
        bail!("verified invocation was not inactive after graceful stop: {inactive:?}");
    }
    if !store.remove(&logical_id)? {
        bail!("temporary receipt was not removed after graceful stop");
    }

    cleanup.disarm();
    Ok(())
}

fn require_value<T>(name: &str, value: &MetricValue<T>) -> Result<()> {
    match value {
        MetricValue::Value(_) => Ok(()),
        MetricValue::Unsupported => bail!("{name} is unsupported"),
        MetricValue::Unavailable(reason) => bail!("{name} is unavailable: {reason}"),
        MetricValue::Error(reason) => bail!("{name} could not be read: {reason}"),
    }
}

fn require_target_absent(receipt: &BindingReceipt) -> Result<()> {
    for _ in 0..40 {
        let present = match &receipt.mux_target {
            MuxTarget::TmuxPrivate { socket, session } => {
                TmuxAdapter::with_socket(socket).has_session(session)? || socket.exists()
            }
            MuxTarget::Zellij {
                session,
                runtime_dir: Some(runtime_dir),
            } => ZellijAdapter::with_runtime_dir(runtime_dir).has_session(session)?,
            MuxTarget::Zellij {
                runtime_dir: None, ..
            } => bail!("supervised Zellij receipt is missing its exact runtime directory"),
            MuxTarget::TmuxShared { .. } => {
                bail!("live supervision test unexpectedly received a shared tmux target")
            }
        };
        if !present {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(50));
    }
    bail!("private multiplexer target remained after graceful stop")
}

fn graceful_stop_target(receipt: &BindingReceipt) -> Result<()> {
    match &receipt.mux_target {
        MuxTarget::TmuxPrivate { socket, session } => {
            TmuxAdapter::with_socket(socket).kill(session)
        }
        MuxTarget::Zellij {
            session,
            runtime_dir: Some(runtime_dir),
        } => ZellijAdapter::with_runtime_dir(runtime_dir).kill(session),
        MuxTarget::Zellij {
            runtime_dir: None, ..
        } => bail!("supervised Zellij receipt is missing its exact runtime directory"),
        MuxTarget::TmuxShared { .. } => {
            bail!("live supervision test unexpectedly received a shared tmux target")
        }
    }
}
