#![cfg(target_os = "linux")]

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use portagenty::domain::Session;
use portagenty::mux::{Multiplexer, TmuxAdapter, ZellijAdapter};
use portagenty::supervision::{
    BindingReceipt, CapabilityState, LinuxSystemdBackend, LogicalSessionId, MetricValue, MuxTarget,
    OwnershipState, ReceiptStore, ResourceLimits, SupervisionBackend,
};
use uuid::Uuid;

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
    let backend = LinuxSystemdBackend::connect_with_workload_executable(PathBuf::from(env!(
        "CARGO_BIN_EXE_pa"
    )))
    .context("connecting to systemd user manager")?;
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
    let fake_bin = temp.path().join("bin");
    std::fs::create_dir(&fake_bin).context("creating synthetic executable directory")?;
    let fake_claude = fake_bin.join("claude");
    std::fs::write(
        &fake_claude,
        "#!/usr/bin/env bash\nprintf 'started\\n' > \"$FAKE_CLAUDE_STARTED\"\nsleep 300 & child=$!\nwait \"$child\"\n",
    )
    .context("writing synthetic Claude executable")?;
    std::fs::set_permissions(&fake_claude, std::fs::Permissions::from_mode(0o755))
        .context("making synthetic Claude executable runnable")?;
    let wrapper = find_on_path("claude-contained")?;
    let wrapper_log = temp.path().join("claude-contained.log");
    let started = temp.path().join("fake-claude.started");
    let mut environment = BTreeMap::new();
    environment.insert(
        "PATH".into(),
        format!(
            "{}:{}",
            fake_bin.display(),
            std::env::var("PATH").unwrap_or_default()
        ),
    );
    environment.insert("FAKE_CLAUDE_STARTED".into(), started.display().to_string());
    let session = Session {
        name: logical_id.session_name.clone(),
        cwd: workdir,
        command: format!(
            "{} --synthetic > {} 2>&1; printf 'wrapper-failed:%s\\n' \"$?\" >> {}; sleep 300",
            shell_quote(&wrapper),
            shell_quote(&wrapper_log),
            shell_quote(&wrapper_log)
        ),
        kind: Some(portagenty::domain::SessionKind::ClaudeCode),
        env: environment,
        description: Some("synthetic supervision integration test".into()),
    };
    let limits = ResourceLimits {
        memory_high_bytes: Some(256 * 1024 * 1024),
        memory_max_bytes: Some(512 * 1024 * 1024),
        memory_swap_max_bytes: Some(128 * 1024 * 1024),
        cpu_quota_percent: Some(100.0),
        tasks_max: Some(256),
    };

    let receipt_result = match kind {
        MuxKind::Tmux => {
            backend.create_tmux_binding(&store, logical_id.clone(), &session, limits.clone())
        }
        MuxKind::Zellij => backend.create_zellij_binding(
            &store,
            logical_id.clone(),
            "systemd-e2e",
            &session,
            limits.clone(),
        ),
    };
    let receipt = receipt_result.with_context(|| {
        format!(
            "synthetic Claude started={} wrapper-log={}",
            started.exists(),
            std::fs::read_to_string(&wrapper_log).unwrap_or_else(|_| "<absent>".into())
        )
    })?;
    cleanup.arm(receipt.clone());

    for _ in 0..40 {
        if started.exists() {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }
    if !started.exists() {
        bail!(
            "claude-contained did not execute the synthetic Claude binary: {}",
            std::fs::read_to_string(&wrapper_log).unwrap_or_else(|_| "<no wrapper log>".into())
        );
    }

    if receipt.logical_id != logical_id {
        bail!("receipt logical identity does not match the synthetic session");
    }
    if receipt.limits != limits {
        bail!("receipt does not preserve the requested resource limits");
    }
    if store.find(&logical_id)?.as_ref() != Some(&receipt) {
        bail!("temporary receipt store does not contain the created binding");
    }
    verify_claude_service_evidence(&receipt, &limits)?;
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

fn find_on_path(name: &str) -> Result<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| anyhow::anyhow!("{name} is not available on PATH"))
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

fn verify_claude_service_evidence(
    receipt: &BindingReceipt,
    expected: &ResourceLimits,
) -> Result<()> {
    if receipt.schema_version != portagenty::supervision::model::RECEIPT_SCHEMA_VERSION {
        bail!("new launch did not persist v2 receipt evidence");
    }
    if receipt.requested_slice.as_deref() != Some("claude-code.slice") {
        bail!("Claude test service did not request claude-code.slice");
    }
    let control_group_path = std::path::Path::new(&receipt.control_group);
    if !receipt
        .control_group
        .contains("/claude.slice/claude-code.slice/")
        || control_group_path
            .parent()
            .and_then(std::path::Path::file_name)
            .and_then(|name| name.to_str())
            != Some("claude-code.slice")
        || receipt.control_group.contains("/run-")
    {
        bail!(
            "Claude root is not a direct service leaf beneath claude-code.slice: {}",
            receipt.control_group
        );
    }

    let anchor = receipt
        .workload_anchor
        .as_ref()
        .context("v2 receipt is missing workload-anchor proof")?;
    let root_cgroup = proc_cgroup(anchor.pid)?;
    if root_cgroup != receipt.control_group {
        bail!(
            "workload root escaped service cgroup: expected {}, got {}",
            receipt.control_group,
            root_cgroup
        );
    }

    let descendants = wait_for_descendants(anchor.pid)?;
    for pid in descendants {
        let actual = proc_cgroup(pid)?;
        if actual != receipt.control_group {
            bail!(
                "descendant PID {pid} escaped service cgroup: expected {}, got {}",
                receipt.control_group,
                actual
            );
        }
    }

    let cgroup =
        PathBuf::from("/sys/fs/cgroup").join(receipt.control_group.trim_start_matches('/'));
    require_cgroup_limit(
        &cgroup,
        "memory.high",
        expected
            .memory_high_bytes
            .context("missing expected MemoryHigh")?,
    )?;
    require_cgroup_limit(
        &cgroup,
        "memory.max",
        expected
            .memory_max_bytes
            .context("missing expected MemoryMax")?,
    )?;
    require_cgroup_limit(
        &cgroup,
        "memory.swap.max",
        expected
            .memory_swap_max_bytes
            .context("missing expected MemorySwapMax")?,
    )?;
    require_cgroup_limit(
        &cgroup,
        "pids.max",
        expected.tasks_max.context("missing expected TasksMax")?,
    )?;
    require_cpu_quota(
        &cgroup,
        expected
            .cpu_quota_percent
            .context("missing expected CPU quota")?,
    )?;
    Ok(())
}

fn wait_for_descendants(root: u32) -> Result<Vec<u32>> {
    for _ in 0..40 {
        let children = proc_descendants(root)?;
        if !children.is_empty() {
            return Ok(children);
        }
        thread::sleep(Duration::from_millis(25));
    }
    bail!("synthetic workload did not retain an ordinary descendant")
}

fn proc_descendants(root: u32) -> Result<Vec<u32>> {
    let mut queue = std::collections::VecDeque::from([root]);
    let mut seen = std::collections::HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(pid) = queue.pop_front() {
        let task_dir = PathBuf::from(format!("/proc/{pid}/task"));
        let entries = match std::fs::read_dir(&task_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && pid != root => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", task_dir.display()))
            }
        };
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", task_dir.display()))?;
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|tid| tid.parse::<u32>().ok())
            else {
                continue;
            };
            let children_path = task_dir.join(tid.to_string()).join("children");
            let raw = match std::fs::read_to_string(&children_path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading {}", children_path.display()))
                }
            };
            for child in raw.split_whitespace() {
                let child = child
                    .parse::<u32>()
                    .with_context(|| format!("parsing child PID in {}", children_path.display()))?;
                if seen.insert(child) {
                    descendants.push(child);
                    queue.push_back(child);
                }
            }
        }
    }
    Ok(descendants)
}

fn proc_cgroup(pid: u32) -> Result<String> {
    let raw = std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .with_context(|| format!("reading cgroup for PID {pid}"))?;
    raw.lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
        .with_context(|| format!("PID {pid} has no unified cgroup-v2 entry"))
}

fn require_cgroup_limit(cgroup: &std::path::Path, file: &str, expected: u64) -> Result<()> {
    let raw = std::fs::read_to_string(cgroup.join(file))
        .with_context(|| format!("reading {file} from {}", cgroup.display()))?;
    let actual = raw
        .trim()
        .parse::<u64>()
        .with_context(|| format!("parsing {file} value {raw:?}"))?;
    if actual != expected {
        bail!("{file} mismatch: expected {expected}, got {actual}");
    }
    Ok(())
}

fn require_cpu_quota(cgroup: &std::path::Path, expected_percent: f64) -> Result<()> {
    let raw = std::fs::read_to_string(cgroup.join("cpu.max"))
        .with_context(|| format!("reading cpu.max from {}", cgroup.display()))?;
    let mut fields = raw.split_whitespace();
    let quota = fields
        .next()
        .context("cpu.max is missing quota")?
        .parse::<u64>()
        .context("parsing cpu.max quota")?;
    let period = fields
        .next()
        .context("cpu.max is missing period")?
        .parse::<u64>()
        .context("parsing cpu.max period")?;
    let actual_percent = quota as f64 / period as f64 * 100.0;
    if (actual_percent - expected_percent).abs() > f64::EPSILON {
        bail!("cpu.max mismatch: expected {expected_percent}%, got {actual_percent}% ({raw:?})");
    }
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
