use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::domain::Session;
use crate::mux::{Multiplexer, TmuxAdapter};

use super::metrics::{
    cgroup_fs_path, counter_rate, cpu_percent, parse_cgroup_state, parse_cpu_stat, parse_io_stat,
    parse_keyed_u64, parse_psi, parse_single_u64,
};
use super::model::{
    ActionKind, ActionResult, ActionStage, BackendKind, BindingReceipt, CapabilityReport,
    CapabilityState, LimitKind, LogicalSessionId, MetricKind, MetricValue, MuxTarget,
    OwnershipState, ResourceSnapshot, SoftLimits, RECEIPT_SCHEMA_VERSION,
};
use super::store::ReceiptStore;
use super::SupervisionBackend;

const SYSTEMD_DESTINATION: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const START_TIMEOUT: Duration = Duration::from_secs(3);
const TARGET_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_TIMEOUT_USEC: u64 = 8_000_000;
const STOP_OBSERVE_TIMEOUT: Duration = Duration::from_secs(9);
const KILL_OBSERVE_TIMEOUT: Duration = Duration::from_secs(3);
const UINT64_MAX: u64 = u64::MAX;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagerInfo {
    pub version: String,
    pub control_group: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnitIdentity {
    pub unit_name: String,
    pub invocation_id: Vec<u8>,
    pub control_group: String,
    pub active_state: String,
    pub sub_state: String,
    pub transient: bool,
}

#[derive(Debug)]
pub struct PtyStdio {
    _master: OwnedFd,
    _slave: OwnedFd,
    tty_path: PathBuf,
}

#[derive(Debug)]
pub struct TransientServiceSpec {
    pub unit_name: String,
    pub executable: PathBuf,
    /// Arguments after argv[0]. The D-Bus adapter prepends the executable.
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: Vec<String>,
    pub limits: SoftLimits,
    pub pty_stdio: Option<PtyStdio>,
}

#[derive(Debug)]
pub struct PreparedLaunch {
    pub logical_id: LogicalSessionId,
    pub spec: TransientServiceSpec,
    pub mux_target: MuxTarget,
    cleanup_paths: Vec<PathBuf>,
}

impl Drop for PreparedLaunch {
    fn drop(&mut self) {
        for path in &self.cleanup_paths {
            let _ = fs::remove_file(path);
        }
    }
}

pub trait SystemdApi: Send + Sync {
    fn manager_info(&self) -> Result<ManagerInfo>;
    fn start_transient_service(&self, spec: &TransientServiceSpec) -> Result<SystemdUnitIdentity>;
    fn unit_by_invocation_id(&self, invocation_id: &[u8]) -> Result<Option<SystemdUnitIdentity>>;
    fn stop_unit(&self, unit_name: &str) -> Result<()>;
    fn kill_unit(&self, unit_name: &str) -> Result<()>;
}

pub struct DbusSystemdApi {
    connection: Connection,
}

impl DbusSystemdApi {
    pub fn connect() -> Result<Self> {
        let connection = Connection::session().context("connecting to the systemd user D-Bus")?;
        Ok(Self { connection })
    }

    fn manager_proxy(&self) -> Result<Proxy<'_>> {
        Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            SYSTEMD_PATH,
            MANAGER_INTERFACE,
        )
        .context("creating systemd manager proxy")
    }

    fn unit_identity(&self, path: OwnedObjectPath) -> Result<SystemdUnitIdentity> {
        let unit = Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            path.as_str(),
            UNIT_INTERFACE,
        )
        .context("creating systemd unit proxy")?;
        let service = Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            path.as_str(),
            SERVICE_INTERFACE,
        )
        .context("creating systemd service proxy")?;
        Ok(SystemdUnitIdentity {
            unit_name: unit.get_property("Id").context("reading systemd unit Id")?,
            invocation_id: unit
                .get_property("InvocationID")
                .context("reading systemd InvocationID")?,
            control_group: service
                .get_property("ControlGroup")
                .context("reading systemd ControlGroup")?,
            active_state: unit
                .get_property("ActiveState")
                .context("reading systemd ActiveState")?,
            sub_state: unit
                .get_property("SubState")
                .context("reading systemd SubState")?,
            transient: unit
                .get_property("Transient")
                .context("reading systemd Transient property")?,
        })
    }

    fn unit_by_name(&self, name: &str) -> Result<Option<SystemdUnitIdentity>> {
        let manager = self.manager_proxy()?;
        let path: OwnedObjectPath = match manager.call("GetUnit", &(name,)) {
            Ok(path) => path,
            Err(error) if is_no_such_unit(&error) => return Ok(None),
            Err(error) => return Err(error).context("resolving systemd unit by name"),
        };
        self.unit_identity(path).map(Some)
    }
}

impl SystemdApi for DbusSystemdApi {
    fn manager_info(&self) -> Result<ManagerInfo> {
        let manager = self.manager_proxy()?;
        Ok(ManagerInfo {
            version: manager
                .get_property("Version")
                .context("reading systemd manager version")?,
            control_group: manager
                .get_property("ControlGroup")
                .context("reading systemd manager control group")?,
        })
    }

    fn start_transient_service(&self, spec: &TransientServiceSpec) -> Result<SystemdUnitIdentity> {
        let manager = self.manager_proxy()?;
        let properties = service_properties(spec)?;
        let aux: Vec<(String, Vec<(String, OwnedValue)>)> = Vec::new();
        let _: OwnedObjectPath = manager
            .call(
                "StartTransientUnit",
                &(spec.unit_name.as_str(), "fail", properties, aux),
            )
            .with_context(|| format!("starting transient unit {:?}", spec.unit_name))?;

        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if let Some(identity) = self.unit_by_name(&spec.unit_name)? {
                if !identity.invocation_id.is_empty() && !identity.control_group.is_empty() {
                    return Ok(identity);
                }
            }
            if Instant::now() >= deadline {
                bail!(
                    "transient unit {:?} did not expose an invocation id and control group within {:?}",
                    spec.unit_name,
                    START_TIMEOUT
                );
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn unit_by_invocation_id(&self, invocation_id: &[u8]) -> Result<Option<SystemdUnitIdentity>> {
        let manager = self.manager_proxy()?;
        let path: OwnedObjectPath = match manager.call("GetUnitByInvocationID", &(invocation_id,)) {
            Ok(path) => path,
            Err(error) if is_no_such_unit(&error) => return Ok(None),
            Err(error) => {
                return Err(error).context("resolving systemd unit by invocation id");
            }
        };
        self.unit_identity(path).map(Some)
    }

    fn stop_unit(&self, unit_name: &str) -> Result<()> {
        let manager = self.manager_proxy()?;
        let _: OwnedObjectPath = manager
            .call("StopUnit", &(unit_name, "fail"))
            .with_context(|| format!("stopping systemd unit {unit_name:?}"))?;
        Ok(())
    }

    fn kill_unit(&self, unit_name: &str) -> Result<()> {
        let manager = self.manager_proxy()?;
        let _: () = manager
            .call("KillUnit", &(unit_name, "all", 9_i32))
            .with_context(|| format!("force-killing systemd unit {unit_name:?}"))?;
        Ok(())
    }
}

fn is_no_such_unit(error: &zbus::Error) -> bool {
    let text = error.to_string();
    text.contains("org.freedesktop.systemd1.NoSuchUnit")
        || text.contains("org.freedesktop.systemd1.NoUnitForInvocationID")
        || text.contains("not loaded")
}

fn service_properties(spec: &TransientServiceSpec) -> Result<Vec<(String, OwnedValue)>> {
    let executable = path_to_utf8(&spec.executable, "multiplexer executable")?;
    let working_directory = path_to_utf8(&spec.working_directory, "working directory")?;
    let mut argv = Vec::with_capacity(spec.args.len() + 1);
    argv.push(executable.clone());
    argv.extend(spec.args.iter().cloned());
    let exec_start = vec![(executable, argv, false)];

    let mut properties = vec![
        property("Description", "Portagenty supervised workload".to_string())?,
        property("Type", "exec".to_string())?,
        property("ExitType", "cgroup".to_string())?,
        property("KillMode", "control-group".to_string())?,
        property("SendSIGKILL", false)?,
        property("Restart", "no".to_string())?,
        // If the kernel OOM-kills one child, preserve the remaining session
        // processes so Portagenty can report the event instead of turning it
        // into an implicit cgroup-wide shutdown.
        property("OOMPolicy", "continue".to_string())?,
        property("CPUAccounting", true)?,
        property("MemoryAccounting", true)?,
        property("IOAccounting", true)?,
        property("TasksAccounting", true)?,
        property("CollectMode", "inactive-or-failed".to_string())?,
        property("TimeoutStopUSec", STOP_TIMEOUT_USEC)?,
        property("WorkingDirectory", working_directory)?,
        property("Environment", spec.environment.clone())?,
        property("ExecStart", exec_start)?,
    ];
    if let Some(pty) = &spec.pty_stdio {
        let tty_path = path_to_utf8(&pty.tty_path, "PTY slave path")?;
        properties.push(property("StandardInput", "tty".to_string())?);
        properties.push(property("StandardOutput", "tty".to_string())?);
        properties.push(property("StandardError", "tty".to_string())?);
        properties.push(property("TTYPath", tty_path)?);
    }
    if let Some(value) = spec.limits.memory_high_bytes {
        properties.push(property("MemoryHigh", value)?);
    }
    if let Some(percent) = spec.limits.cpu_quota_percent {
        let usec = (percent * 10_000.0).round();
        if !usec.is_finite() || usec <= 0.0 || usec > u64::MAX as f64 {
            bail!("CPU quota {percent} cannot be represented by systemd");
        }
        properties.push(property("CPUQuotaPerSecUSec", usec as u64)?);
    }
    if let Some(value) = spec.limits.tasks_max {
        properties.push(property("TasksMax", value)?);
    }
    Ok(properties)
}

fn property<T>(name: &str, value: T) -> Result<(String, OwnedValue)>
where
    T: zbus::zvariant::DynamicType + Into<Value<'static>>,
{
    owned_property(name, Value::new(value))
}

fn owned_property(name: &str, value: Value<'static>) -> Result<(String, OwnedValue)> {
    Ok((
        name.to_string(),
        value
            .try_to_owned()
            .with_context(|| format!("owning D-Bus property {name}"))?,
    ))
}

pub struct LinuxSystemdBackend {
    api: Arc<dyn SystemdApi>,
    cgroup_root: PathBuf,
}

impl LinuxSystemdBackend {
    pub fn connect() -> Result<Self> {
        Ok(Self {
            api: Arc::new(DbusSystemdApi::connect()?),
            cgroup_root: PathBuf::from(CGROUP_ROOT),
        })
    }

    #[cfg(test)]
    pub fn with_api(api: Arc<dyn SystemdApi>, cgroup_root: PathBuf) -> Self {
        Self { api, cgroup_root }
    }

    pub fn prepare_tmux_launch(
        &self,
        logical_id: LogicalSessionId,
        session: &Session,
        limits: SoftLimits,
    ) -> Result<PreparedLaunch> {
        self.require_limit_capabilities(&limits)?;
        let runtime_dir = validated_runtime_dir()?;
        let names = super::model::generate_names(&logical_id, uuid::Uuid::new_v4(), &runtime_dir)?;
        ensure_private_runtime_dir(
            names
                .tmux_socket
                .parent()
                .ok_or_else(|| anyhow!("generated tmux socket has no parent"))?,
        )?;
        let adapter = TmuxAdapter::with_socket(&names.tmux_socket);
        if adapter.has_session(&names.tmux_session)? {
            bail!("generated private tmux target unexpectedly already exists");
        }
        let args = adapter.create_detached_args(session, &names.tmux_session)?;
        let args = os_args_to_utf8(args)?;
        let executable = resolve_executable("tmux")?;
        Ok(PreparedLaunch {
            logical_id,
            mux_target: MuxTarget::TmuxPrivate {
                socket: names.tmux_socket,
                session: names.tmux_session,
            },
            cleanup_paths: Vec::new(),
            spec: TransientServiceSpec {
                unit_name: names.unit_name,
                executable,
                args,
                working_directory: session.cwd.clone(),
                environment: sanitized_environment(&session.env)?,
                limits,
                pty_stdio: None,
            },
        })
    }

    pub fn prepare_zellij_launch(
        &self,
        logical_id: LogicalSessionId,
        workspace_name: &str,
        session: &Session,
        limits: SoftLimits,
    ) -> Result<PreparedLaunch> {
        self.require_limit_capabilities(&limits)?;
        if !session.cwd.is_dir() {
            bail!("session cwd does not exist: {}", session.cwd.display());
        }
        let runtime_dir = validated_runtime_dir()?;
        let names = super::model::generate_names(&logical_id, uuid::Uuid::new_v4(), &runtime_dir)?;
        let layout_dir = runtime_dir.join("portagenty/zellij");
        ensure_private_runtime_dir(&layout_dir)?;
        let layout_path = layout_dir.join(format!("{}.kdl", names.zellij_session));
        let adapter = crate::mux::ZellijAdapter::with_runtime_dir(&runtime_dir);
        if adapter.has_session(&names.zellij_session)? {
            bail!("generated Zellij target unexpectedly already exists");
        }
        let executable = resolve_executable("zellij")?;
        let layout_arg = path_to_utf8(&layout_path, "Zellij layout path")?;
        let environment = sanitized_environment_with_runtime(&session.env, &runtime_dir)?;
        let pty_stdio = open_pty_stdio()?;
        let tab_name = format!("{workspace_name} / {}", session.name);
        write_private_file(
            &layout_path,
            crate::mux::zellij::render_layout_with_tab_name(session, &tab_name).as_bytes(),
        )?;

        Ok(PreparedLaunch {
            logical_id,
            mux_target: MuxTarget::Zellij {
                session: names.zellij_session.clone(),
                runtime_dir: Some(runtime_dir),
            },
            cleanup_paths: vec![layout_path],
            spec: TransientServiceSpec {
                unit_name: names.unit_name,
                executable,
                args: vec![
                    "--session".into(),
                    names.zellij_session,
                    "--new-session-with-layout".into(),
                    layout_arg,
                ],
                working_directory: session.cwd.clone(),
                environment,
                limits,
                pty_stdio: Some(pty_stdio),
            },
        })
    }

    pub fn start_prepared(&self, prepared: &PreparedLaunch) -> Result<BindingReceipt> {
        let identity = self.api.start_transient_service(&prepared.spec)?;
        let launch_result = (|| {
            if identity.unit_name != prepared.spec.unit_name {
                bail!("systemd started a different unit than Portagenty requested");
            }
            if !identity.transient {
                bail!("systemd did not mark the new workload as transient");
            }
            if identity.invocation_id.len() != 16 {
                bail!("systemd returned an invalid invocation id");
            }
            if identity.control_group.is_empty() {
                bail!("systemd returned an empty control group");
            }
            let manager = self.api.manager_info()?;
            self.validated_cgroup_path(&manager.control_group, &identity.control_group)?;
            wait_for_mux_target(&prepared.mux_target, TARGET_TIMEOUT)?;
            let receipt = BindingReceipt {
                schema_version: RECEIPT_SCHEMA_VERSION,
                logical_id: prepared.logical_id.clone(),
                backend: BackendKind::SystemdUserService,
                unit_name: identity.unit_name.clone(),
                invocation_id: encode_hex(&identity.invocation_id),
                control_group: identity.control_group.clone(),
                mux_target: prepared.mux_target.clone(),
                observed_at_unix_ms: now_unix_ms(),
                limits: prepared.spec.limits.clone(),
            };
            receipt.validate_shape()?;
            Ok(receipt)
        })();
        if launch_result.is_err() {
            let _ = self.api.stop_unit(&prepared.spec.unit_name);
        }
        launch_result.context("validating the new supervised workload")
    }

    /// Create and receipt one private tmux server while holding the receipt
    /// store lock across the ownership decision. A persistence failure stops
    /// the just-created unit so Portagenty never leaves an unreceipted owned
    /// workload behind.
    pub fn create_tmux_binding(
        &self,
        store: &ReceiptStore,
        logical_id: LogicalSessionId,
        session: &Session,
        limits: SoftLimits,
    ) -> Result<BindingReceipt> {
        let mut started = None;
        let result = store.update_locked(|file| {
            if let Some(existing) = file
                .bindings
                .iter()
                .find(|receipt| receipt.logical_id == logical_id)
            {
                return match self.reconcile(existing)? {
                    OwnershipState::OwnedVerified(_) => Ok(existing.clone()),
                    OwnershipState::StaleBinding(reason) => bail!(
                        "a stale supervision receipt already exists for this session: {reason}"
                    ),
                    state => bail!(
                        "an incompatible supervision receipt already exists for this session: {state:?}"
                    ),
                };
            }

            let prepared =
                self.prepare_tmux_launch(logical_id.clone(), session, limits.clone())?;
            let receipt = self.start_prepared(&prepared)?;
            started = Some(receipt.clone());
            file.bindings.push(receipt.clone());
            file.bindings.sort_by(|left, right| left.logical_id.cmp(&right.logical_id));
            Ok(receipt)
        });
        if result.is_err() {
            if let Some(receipt) = &started {
                let _ = self.api.stop_unit(&receipt.unit_name);
                if let MuxTarget::TmuxPrivate { socket, .. } = &receipt.mux_target {
                    let _ = fs::remove_file(socket);
                }
            }
        }
        result.context("creating supervised tmux binding")
    }

    pub fn create_zellij_binding(
        &self,
        store: &ReceiptStore,
        logical_id: LogicalSessionId,
        workspace_name: &str,
        session: &Session,
        limits: SoftLimits,
    ) -> Result<BindingReceipt> {
        let mut started = None;
        let result = store.update_locked(|file| {
            if let Some(existing) = file
                .bindings
                .iter()
                .find(|receipt| receipt.logical_id == logical_id)
            {
                return match self.reconcile(existing)? {
                    OwnershipState::OwnedVerified(_) => Ok(existing.clone()),
                    OwnershipState::StaleBinding(reason) => bail!(
                        "a stale supervision receipt already exists for this session: {reason}"
                    ),
                    state => bail!(
                        "an incompatible supervision receipt already exists for this session: {state:?}"
                    ),
                };
            }

            let prepared = self.prepare_zellij_launch(
                logical_id.clone(),
                workspace_name,
                session,
                limits.clone(),
            )?;
            let receipt = self.start_prepared(&prepared)?;
            started = Some(receipt.clone());
            file.bindings.push(receipt.clone());
            file.bindings.sort_by(|left, right| left.logical_id.cmp(&right.logical_id));
            Ok(receipt)
        });
        if result.is_err() {
            if let Some(receipt) = &started {
                let _ = self.api.stop_unit(&receipt.unit_name);
            }
        }
        result.context("creating supervised Zellij binding")
    }

    fn require_limit_capabilities(&self, limits: &SoftLimits) -> Result<()> {
        if limits.is_empty() {
            return Ok(());
        }
        let report = self.capabilities();
        for (requested, kind) in [
            (limits.memory_high_bytes.is_some(), LimitKind::MemoryHigh),
            (limits.cpu_quota_percent.is_some(), LimitKind::CpuQuota),
            (limits.tasks_max.is_some(), LimitKind::TasksMax),
        ] {
            if requested && report.limits.get(&kind) != Some(&CapabilityState::Supported) {
                bail!("requested {kind:?} guardrail is not supported on this host");
            }
        }
        Ok(())
    }

    fn verified_binding(&self, receipt: &BindingReceipt) -> Result<SystemdUnitIdentity> {
        let identity = self.verified_identity(receipt)?;
        if !mux_target_exists(&receipt.mux_target)? {
            bail!("recorded multiplexer target is no longer present");
        }
        Ok(identity)
    }

    fn verified_identity(&self, receipt: &BindingReceipt) -> Result<SystemdUnitIdentity> {
        self.identity_if_present(receipt)?
            .ok_or_else(|| anyhow!("owned systemd unit is no longer present"))
    }

    fn identity_if_present(&self, receipt: &BindingReceipt) -> Result<Option<SystemdUnitIdentity>> {
        receipt.validate_shape()?;
        let invocation_id = decode_hex(&receipt.invocation_id)?;
        if invocation_id.len() != 16 {
            bail!("receipt invocation id is not 16 bytes");
        }
        let Some(identity) = self.api.unit_by_invocation_id(&invocation_id)? else {
            return Ok(None);
        };
        if !identity.transient {
            bail!("receipt resolved to a non-transient unit");
        }
        if identity.unit_name != receipt.unit_name {
            bail!("systemd unit name no longer matches the receipt");
        }
        if identity.invocation_id != invocation_id {
            bail!("systemd invocation id no longer matches the receipt");
        }
        if identity.control_group != receipt.control_group {
            bail!("systemd control group no longer matches the receipt");
        }
        let manager = self.api.manager_info()?;
        self.validated_cgroup_path(&manager.control_group, &identity.control_group)?;
        Ok(Some(identity))
    }

    fn wait_for_unit_exit(&self, invocation_id: &[u8], timeout: Duration) -> Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.api.unit_by_invocation_id(invocation_id)? {
                None => return Ok(true),
                Some(identity)
                    if matches!(identity.active_state.as_str(), "inactive" | "failed") =>
                {
                    return Ok(true);
                }
                Some(_) if Instant::now() >= deadline => return Ok(false),
                Some(_) => thread::sleep(Duration::from_millis(50)),
            }
        }
    }

    fn validated_cgroup_path(&self, manager: &str, unit: &str) -> Result<PathBuf> {
        let path = cgroup_fs_path(&self.cgroup_root, manager, unit)?;
        let root = fs::canonicalize(&self.cgroup_root)
            .with_context(|| format!("canonicalizing {}", self.cgroup_root.display()))?;
        let canonical = fs::canonicalize(&path)
            .with_context(|| format!("canonicalizing unit cgroup {}", path.display()))?;
        if !canonical.starts_with(&root) {
            bail!("unit cgroup escaped the cgroup-v2 mount");
        }
        Ok(canonical)
    }
}

impl SupervisionBackend for LinuxSystemdBackend {
    fn capabilities(&self) -> CapabilityReport {
        capability_report(self.api.as_ref(), &self.cgroup_root)
    }

    fn reconcile(&self, receipt: &BindingReceipt) -> Result<OwnershipState> {
        match self.verified_binding(receipt) {
            Ok(_) => Ok(OwnershipState::OwnedVerified(Box::new(receipt.clone()))),
            Err(error) => Ok(OwnershipState::StaleBinding(format!("{error:#}"))),
        }
    }

    fn snapshot(
        &self,
        receipt: &BindingReceipt,
        previous: Option<&ResourceSnapshot>,
    ) -> Result<ResourceSnapshot> {
        let identity = self.verified_binding(receipt)?;
        let manager = self.api.manager_info()?;
        let path = self.validated_cgroup_path(&manager.control_group, &identity.control_group)?;
        Ok(read_snapshot(&path, previous))
    }

    fn stop_unit(&self, receipt: &BindingReceipt) -> Result<ActionResult> {
        if self.identity_if_present(receipt)?.is_none() {
            return Ok(ActionResult {
                logical_id: receipt.logical_id.clone(),
                requested: ActionStage::SystemdStop,
                attempted: Vec::new(),
                completed: true,
                final_state: "verified invocation was already inactive".into(),
            });
        }
        let invocation_id = decode_hex(&receipt.invocation_id)?;
        self.api.stop_unit(&receipt.unit_name)?;
        let completed = self.wait_for_unit_exit(&invocation_id, STOP_OBSERVE_TIMEOUT)?;
        Ok(ActionResult {
            logical_id: receipt.logical_id.clone(),
            requested: ActionStage::SystemdStop,
            attempted: vec![ActionStage::SystemdStop],
            completed,
            final_state: if completed {
                "verified control group became inactive without force escalation".into()
            } else {
                "systemd stop timed out with SendSIGKILL=no; force escalation was not performed"
                    .into()
            },
        })
    }

    fn force_kill(&self, receipt: &BindingReceipt) -> Result<ActionResult> {
        if self.identity_if_present(receipt)?.is_none() {
            return Ok(ActionResult {
                logical_id: receipt.logical_id.clone(),
                requested: ActionStage::ForceKill,
                attempted: Vec::new(),
                completed: true,
                final_state: "verified invocation was already inactive".into(),
            });
        }
        let invocation_id = decode_hex(&receipt.invocation_id)?;
        self.api.kill_unit(&receipt.unit_name)?;
        let completed = self.wait_for_unit_exit(&invocation_id, KILL_OBSERVE_TIMEOUT)?;
        Ok(ActionResult {
            logical_id: receipt.logical_id.clone(),
            requested: ActionStage::ForceKill,
            attempted: vec![ActionStage::ForceKill],
            completed,
            final_state: if completed {
                "verified control group became inactive after explicit force-kill".into()
            } else {
                "force-kill was sent, but the verified control group remained active".into()
            },
        })
    }
}

fn capability_report(api: &dyn SystemdApi, cgroup_root: &Path) -> CapabilityReport {
    let mut metrics = BTreeMap::new();
    let mut actions = BTreeMap::new();
    let mut limits = BTreeMap::new();
    let mut notes = Vec::new();
    let manager = match api.manager_info() {
        Ok(info) => info,
        Err(error) => {
            return CapabilityReport::unsupported(format!(
                "systemd user manager is unavailable: {error:#}"
            ));
        }
    };
    let version = parse_systemd_version(&manager.version).unwrap_or(0);
    if version < 250 {
        return CapabilityReport::unsupported(format!(
            "systemd {version} is too old; ExitType=cgroup requires systemd 250 or newer"
        ));
    }
    if !manager.control_group.starts_with('/') {
        return CapabilityReport::unsupported(
            "systemd user manager reported an invalid control-group path",
        );
    }
    if !cgroup_root.join("cgroup.controllers").is_file() {
        return CapabilityReport::unsupported("unified cgroup v2 is unavailable");
    }
    let runtime_error = validated_runtime_dir().err();
    let controllers = fs::read_to_string(cgroup_root.join("cgroup.controllers"))
        .unwrap_or_default()
        .split_whitespace()
        .map(str::to_string)
        .collect::<std::collections::HashSet<_>>();

    for kind in MetricKind::ALL {
        let state = match kind {
            MetricKind::CpuUsage | MetricKind::CpuRate | MetricKind::CpuPressure => {
                controller_state(&controllers, "cpu")
            }
            MetricKind::MemoryCurrent
            | MetricKind::MemoryPeak
            | MetricKind::MemoryEvents
            | MetricKind::SwapCurrent
            | MetricKind::SwapPeak
            | MetricKind::SwapEvents
            | MetricKind::MemoryPressure => controller_state(&controllers, "memory"),
            MetricKind::TasksCurrent | MetricKind::TasksPeak | MetricKind::TasksEvents => {
                controller_state(&controllers, "pids")
            }
            MetricKind::IoTotals | MetricKind::IoRates | MetricKind::IoPressure => {
                controller_state(&controllers, "io")
            }
            MetricKind::CgroupState => CapabilityState::Supported,
        };
        metrics.insert(kind, state);
    }
    actions.insert(ActionKind::GracefulStop, CapabilityState::Supported);
    actions.insert(ActionKind::StopUnit, CapabilityState::Supported);
    actions.insert(ActionKind::ForceKill, CapabilityState::Supported);
    limits.insert(
        LimitKind::MemoryHigh,
        controller_state(&controllers, "memory"),
    );
    limits.insert(LimitKind::CpuQuota, controller_state(&controllers, "cpu"));
    limits.insert(LimitKind::TasksMax, controller_state(&controllers, "pids"));
    let overall = if let Some(error) = runtime_error {
        let reason = format!("secure runtime directory is unavailable: {error:#}");
        notes.push(reason.clone());
        CapabilityState::Unavailable(reason)
    } else {
        CapabilityState::Supported
    };
    if crate::find::is_wsl() {
        notes.push("WSL native-Windows child processes can escape Linux cgroup containment".into());
    }
    CapabilityReport {
        backend: BackendKind::SystemdUserService,
        overall,
        metrics,
        actions,
        limits,
        notes,
    }
}

fn controller_state(
    controllers: &std::collections::HashSet<String>,
    controller: &str,
) -> CapabilityState {
    if controllers.contains(controller) {
        CapabilityState::Supported
    } else {
        CapabilityState::Unavailable(format!("cgroup {controller} controller is unavailable"))
    }
}

fn parse_systemd_version(raw: &str) -> Option<u32> {
    raw.split(|character: char| !character.is_ascii_digit())
        .find(|part| !part.is_empty())?
        .parse()
        .ok()
}

fn read_snapshot(path: &Path, previous: Option<&ResourceSnapshot>) -> ResourceSnapshot {
    let captured_at_unix_ms = now_unix_ms();
    let cpu = read_parsed(path.join("cpu.stat"), parse_cpu_stat);
    let memory_current_bytes = read_u64(path.join("memory.current"), "memory.current");
    let memory_peak_bytes = read_u64(path.join("memory.peak"), "memory.peak");
    let memory_events = read_parsed(path.join("memory.events"), |raw| {
        parse_keyed_u64(raw, "memory.events")
    });
    let swap_current_bytes = read_u64(path.join("memory.swap.current"), "memory.swap.current");
    let swap_peak_bytes = read_u64(path.join("memory.swap.peak"), "memory.swap.peak");
    let swap_events = read_parsed(path.join("memory.swap.events"), |raw| {
        parse_keyed_u64(raw, "memory.swap.events")
    });
    let tasks_current = read_u64(path.join("pids.current"), "pids.current");
    let tasks_peak = read_u64(path.join("pids.peak"), "pids.peak");
    let tasks_events = read_parsed(path.join("pids.events"), |raw| {
        parse_keyed_u64(raw, "pids.events")
    });
    let io_totals = read_parsed(path.join("io.stat"), parse_io_stat);
    let cpu_pressure = read_parsed(path.join("cpu.pressure"), parse_psi);
    let memory_pressure = read_parsed(path.join("memory.pressure"), parse_psi);
    let io_pressure = read_parsed(path.join("io.pressure"), parse_psi);
    let cgroup_state = read_parsed(path.join("cgroup.events"), parse_cgroup_state);

    let elapsed = previous.and_then(|old| {
        captured_at_unix_ms
            .checked_sub(old.captured_at_unix_ms)
            .map(Duration::from_millis)
    });
    let cpu_percent_value = match (&cpu, previous.and_then(snapshot_cpu), elapsed) {
        (MetricValue::Value(current), Some(old), Some(elapsed)) => {
            cpu_percent(old.usage_usec, current.usage_usec, elapsed)
                .map(MetricValue::Value)
                .unwrap_or_else(|| {
                    MetricValue::Unavailable("CPU counter reset or no elapsed time".into())
                })
        }
        _ => MetricValue::Unavailable("a previous CPU sample is required".into()),
    };
    let (io_read_bytes_per_sec, io_write_bytes_per_sec) =
        match (&io_totals, previous.and_then(snapshot_io), elapsed) {
            (MetricValue::Value(current), Some(old), Some(elapsed)) => (
                counter_rate(old.read_bytes, current.read_bytes, elapsed)
                    .map(MetricValue::Value)
                    .unwrap_or_else(|| MetricValue::Unavailable("I/O counter reset".into())),
                counter_rate(old.write_bytes, current.write_bytes, elapsed)
                    .map(MetricValue::Value)
                    .unwrap_or_else(|| MetricValue::Unavailable("I/O counter reset".into())),
            ),
            _ => (
                MetricValue::Unavailable("a previous I/O sample is required".into()),
                MetricValue::Unavailable("a previous I/O sample is required".into()),
            ),
        };

    ResourceSnapshot {
        captured_at_unix_ms,
        cpu,
        cpu_percent: cpu_percent_value,
        memory_current_bytes,
        memory_peak_bytes,
        memory_events,
        swap_current_bytes,
        swap_peak_bytes,
        swap_events,
        tasks_current,
        tasks_peak,
        tasks_events,
        io_totals,
        io_read_bytes_per_sec,
        io_write_bytes_per_sec,
        cpu_pressure,
        memory_pressure,
        io_pressure,
        cgroup_state,
    }
}

fn snapshot_cpu(snapshot: &ResourceSnapshot) -> Option<&super::model::CpuStat> {
    match &snapshot.cpu {
        MetricValue::Value(value) => Some(value),
        _ => None,
    }
}

fn snapshot_io(snapshot: &ResourceSnapshot) -> Option<&super::model::IoTotals> {
    match &snapshot.io_totals {
        MetricValue::Value(value) => Some(value),
        _ => None,
    }
}

fn read_u64(path: PathBuf, label: &str) -> MetricValue<u64> {
    read_parsed(path, |raw| parse_single_u64(raw, label)).map_unavailable_max()
}

fn read_parsed<T>(path: PathBuf, parser: impl FnOnce(&str) -> Result<T>) -> MetricValue<T> {
    match fs::read_to_string(&path) {
        Ok(raw) => match parser(&raw) {
            Ok(value) => MetricValue::Value(value),
            Err(error) => MetricValue::Error(format!("{}: {error:#}", path.display())),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => MetricValue::Unsupported,
        Err(error) => MetricValue::Error(format!("reading {}: {error}", path.display())),
    }
}

trait MetricValueU64Ext {
    fn map_unavailable_max(self) -> Self;
}

impl MetricValueU64Ext for MetricValue<u64> {
    fn map_unavailable_max(self) -> Self {
        match self {
            MetricValue::Value(UINT64_MAX) => {
                MetricValue::Unavailable("systemd reported this metric as unavailable".into())
            }
            other => other,
        }
    }
}

fn wait_for_mux_target(target: &MuxTarget, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if mux_target_exists(target)? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("multiplexer target did not appear within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

pub fn mux_target_exists(target: &MuxTarget) -> Result<bool> {
    match target {
        MuxTarget::TmuxPrivate { socket, session } => {
            TmuxAdapter::with_socket(socket).has_session(session)
        }
        MuxTarget::TmuxShared { session } => TmuxAdapter::new().has_session(session),
        MuxTarget::Zellij {
            session,
            runtime_dir,
        } => match runtime_dir {
            Some(runtime_dir) => {
                crate::mux::ZellijAdapter::with_runtime_dir(runtime_dir).has_session(session)
            }
            None => crate::mux::ZellijAdapter::new().has_session(session),
        },
    }
}

fn open_pty_stdio() -> Result<PtyStdio> {
    use rustix::pty::{grantpt, ioctl_tiocgptpeer, openpt, ptsname, unlockpt, OpenptFlags};
    use rustix::termios::{tcsetwinsize, Winsize};

    let flags = OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC;
    let master = openpt(flags).context("opening PTY master")?;
    grantpt(&master).context("granting PTY slave access")?;
    unlockpt(&master).context("unlocking PTY slave")?;
    let tty_path = PathBuf::from(
        ptsname(&master, Vec::new())
            .context("resolving PTY slave path")?
            .into_string()
            .map_err(|_| anyhow!("PTY slave path is not valid UTF-8"))?,
    );
    let slave = ioctl_tiocgptpeer(&master, flags).context("opening PTY slave")?;
    tcsetwinsize(
        &slave,
        Winsize {
            ws_row: 24,
            ws_col: 80,
            ws_xpixel: 0,
            ws_ypixel: 0,
        },
    )
    .context("initializing PTY window size")?;
    Ok(PtyStdio {
        _master: master,
        _slave: slave,
        tty_path,
    })
}

fn validated_runtime_dir() -> Result<PathBuf> {
    let uid = rustix::process::geteuid().as_raw();
    let candidate = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/run/user").join(uid.to_string()));
    let metadata = fs::symlink_metadata(&candidate)
        .with_context(|| format!("reading runtime directory {}", candidate.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o700
    {
        bail!(
            "runtime directory {} is not a secure mode-0700 directory owned by uid {uid}",
            candidate.display()
        );
    }
    Ok(candidate)
}

fn ensure_private_runtime_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
        .with_context(|| format!("setting private runtime permissions on {}", path.display()))
}

fn write_private_file(path: &Path, contents: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating private file {}", path.display()))?;
    let result = (|| {
        file.write_all(contents)
            .with_context(|| format!("writing private file {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("syncing private file {}", path.display()))
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn sanitized_environment_with_runtime(
    overrides: &BTreeMap<String, String>,
    runtime_dir: &Path,
) -> Result<Vec<String>> {
    let mut environment = sanitized_environment(overrides)?;
    environment.retain(|entry| !entry.starts_with("XDG_RUNTIME_DIR="));
    environment.push(format!(
        "XDG_RUNTIME_DIR={}",
        path_to_utf8(runtime_dir, "runtime directory")?
    ));
    environment.sort();
    Ok(environment)
}

fn sanitized_environment(overrides: &BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut environment: BTreeMap<String, String> = std::env::vars().collect();
    environment.retain(|key, _| !is_stripped_environment_key(key));
    for (key, value) in overrides {
        if key.contains('=') || key.contains('\0') || value.contains('\0') {
            bail!("session environment contains an invalid D-Bus environment entry");
        }
        environment.insert(key.clone(), value.clone());
    }
    Ok(environment
        .into_iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect())
}

fn is_stripped_environment_key(key: &str) -> bool {
    matches!(
        key,
        "TMUX" | "TMUX_PANE" | "ZELLIJ" | "ZELLIJ_SESSION_NAME" | "NOTIFY_SOCKET" | "INVOCATION_ID"
    ) || key.starts_with("LISTEN_")
        || key.starts_with("WATCHDOG_")
}

fn resolve_executable(name: &str) -> Result<PathBuf> {
    let candidate = Path::new(name);
    if candidate.components().count() > 1 {
        return executable_path(candidate);
    }
    let path = std::env::var_os("PATH").ok_or_else(|| anyhow!("PATH is not set"))?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(name);
        if let Ok(path) = executable_path(&candidate) {
            return Ok(path);
        }
    }
    bail!("{name} was not found as an executable on PATH")
}

fn executable_path(path: &Path) -> Result<PathBuf> {
    let metadata = fs::metadata(path).with_context(|| format!("reading {}", path.display()))?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        bail!("{} is not an executable file", path.display());
    }
    fs::canonicalize(path).with_context(|| format!("canonicalizing {}", path.display()))
}

fn os_args_to_utf8(args: Vec<OsString>) -> Result<Vec<String>> {
    args.into_iter()
        .map(|argument| {
            argument
                .into_string()
                .map_err(|_| anyhow!("supervised launch arguments must be valid UTF-8"))
        })
        .collect()
}

fn path_to_utf8(path: &Path, label: &str) -> Result<String> {
    path.to_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{label} must be valid UTF-8 for systemd D-Bus"))
}

fn now_unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        encoded.push(HEX[(byte >> 4) as usize] as char);
        encoded.push(HEX[(byte & 0x0f) as usize] as char);
    }
    encoded
}

fn decode_hex(raw: &str) -> Result<Vec<u8>> {
    if raw.len() % 2 != 0 {
        bail!("invocation id has an odd number of hexadecimal digits");
    }
    raw.as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_nibble(pair[0])?;
            let low = hex_nibble(pair[1])?;
            Ok((high << 4) | low)
        })
        .collect()
}

fn hex_nibble(value: u8) -> Result<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(anyhow!("invalid hexadecimal invocation id")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{Session, SessionKind};
    use std::sync::Mutex;

    #[test]
    fn zellij_pty_has_a_usable_path_and_initial_window_size() {
        let pty = open_pty_stdio().unwrap();
        assert!(pty.tty_path.starts_with("/dev/pts/"));
        let size = rustix::termios::tcgetwinsize(&pty._slave).unwrap();
        assert_eq!(size.ws_row, 24);
        assert_eq!(size.ws_col, 80);
    }

    struct FakeSystemd {
        manager: ManagerInfo,
        unit: Mutex<Option<SystemdUnitIdentity>>,
        stops: Mutex<Vec<String>>,
        kills: Mutex<Vec<String>>,
    }

    impl SystemdApi for FakeSystemd {
        fn manager_info(&self) -> Result<ManagerInfo> {
            Ok(self.manager.clone())
        }

        fn start_transient_service(
            &self,
            _spec: &TransientServiceSpec,
        ) -> Result<SystemdUnitIdentity> {
            self.unit
                .lock()
                .unwrap()
                .clone()
                .ok_or_else(|| anyhow!("no fake unit"))
        }

        fn unit_by_invocation_id(
            &self,
            invocation_id: &[u8],
        ) -> Result<Option<SystemdUnitIdentity>> {
            Ok(self
                .unit
                .lock()
                .unwrap()
                .clone()
                .filter(|unit| unit.invocation_id == invocation_id))
        }

        fn stop_unit(&self, unit_name: &str) -> Result<()> {
            self.stops.lock().unwrap().push(unit_name.to_string());
            Ok(())
        }

        fn kill_unit(&self, unit_name: &str) -> Result<()> {
            self.kills.lock().unwrap().push(unit_name.to_string());
            Ok(())
        }
    }

    fn session() -> Session {
        Session {
            name: "agent".into(),
            cwd: PathBuf::from("/tmp"),
            command: "bash".into(),
            kind: Some(SessionKind::Shell),
            env: BTreeMap::new(),
            description: None,
        }
    }

    #[test]
    fn service_properties_include_soft_limits_and_safe_stop_policy() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            executable: PathBuf::from("/usr/bin/tmux"),
            args: vec!["new-session".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: vec!["PATH=/usr/bin".into()],
            limits: SoftLimits {
                memory_high_bytes: Some(1024),
                cpu_quota_percent: Some(250.0),
                tasks_max: Some(50),
            },
            pty_stdio: None,
        };
        let properties = service_properties(&spec).unwrap();
        let names: std::collections::HashSet<&str> =
            properties.iter().map(|(name, _)| name.as_str()).collect();
        for required in [
            "Type",
            "ExitType",
            "KillMode",
            "SendSIGKILL",
            "OOMPolicy",
            "ExecStart",
            "MemoryHigh",
            "CPUQuotaPerSecUSec",
            "TasksMax",
        ] {
            assert!(names.contains(required), "missing {required}");
        }
    }

    #[test]
    fn pty_stdio_becomes_controlling_terminal_properties() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            executable: PathBuf::from("/usr/bin/zellij"),
            args: vec!["--session".into(), "test".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: vec!["PATH=/usr/bin".into()],
            limits: SoftLimits::default(),
            pty_stdio: Some(open_pty_stdio().unwrap()),
        };
        let properties = service_properties(&spec).unwrap();
        let names: std::collections::HashSet<&str> =
            properties.iter().map(|(name, _)| name.as_str()).collect();
        for required in [
            "StandardInput",
            "StandardOutput",
            "StandardError",
            "TTYPath",
        ] {
            assert!(names.contains(required), "missing {required}");
        }
    }

    #[test]
    fn environment_strips_parent_mux_and_activation_state() {
        assert!(is_stripped_environment_key("TMUX"));
        assert!(is_stripped_environment_key("LISTEN_FDS"));
        assert!(!is_stripped_environment_key("CUSTOM"));
    }

    #[test]
    fn systemd_version_parser_handles_distribution_suffixes() {
        assert_eq!(
            parse_systemd_version("systemd 259 (259.8-1.fc44)"),
            Some(259)
        );
        assert_eq!(parse_systemd_version("250"), Some(250));
    }

    #[test]
    fn invocation_hex_round_trips() {
        let bytes = vec![0, 1, 0xab, 0xff];
        assert_eq!(decode_hex(&encode_hex(&bytes)).unwrap(), bytes);
        assert!(decode_hex("xyz").is_err());
    }

    #[test]
    fn canonical_cgroup_validation_rejects_symlink_escape() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("cgroup");
        let outside = temp.path().join("outside");
        let manager = "/user.slice/user-1000.slice/user@1000.service";
        let parent = root.join(manager.trim_start_matches('/')).join("app.slice");
        fs::create_dir_all(&parent).unwrap();
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, parent.join("example.service")).unwrap();
        let api = Arc::new(FakeSystemd {
            manager: ManagerInfo {
                version: "259".into(),
                control_group: manager.into(),
            },
            unit: Mutex::new(None),
            stops: Mutex::new(Vec::new()),
            kills: Mutex::new(Vec::new()),
        });
        let backend = LinuxSystemdBackend::with_api(api, root);
        assert!(backend
            .validated_cgroup_path(manager, &format!("{manager}/app.slice/example.service"),)
            .is_err());
    }

    #[test]
    fn verified_identity_rejects_invocation_mismatch() {
        let temp = tempfile::tempdir().unwrap();
        let manager_cgroup = "/user.slice/user-1000.slice/user@1000.service";
        let unit_cgroup = format!("{manager_cgroup}/app.slice/example.service");
        let cgroup = temp.path().join(unit_cgroup.trim_start_matches('/'));
        fs::create_dir_all(&cgroup).unwrap();
        let api = Arc::new(FakeSystemd {
            manager: ManagerInfo {
                version: "259".into(),
                control_group: manager_cgroup.into(),
            },
            unit: Mutex::new(Some(SystemdUnitIdentity {
                unit_name: "portagenty-wx-gy.service".into(),
                invocation_id: vec![1, 2, 3],
                control_group: unit_cgroup.clone(),
                active_state: "active".into(),
                sub_state: "running".into(),
                transient: true,
            })),
            stops: Mutex::new(Vec::new()),
            kills: Mutex::new(Vec::new()),
        });
        let backend = LinuxSystemdBackend::with_api(api, temp.path().to_path_buf());
        let receipt = BindingReceipt {
            schema_version: RECEIPT_SCHEMA_VERSION,
            logical_id: LogicalSessionId::new(
                "550e8400-e29b-41d4-a716-446655440000",
                session().name,
            )
            .unwrap(),
            backend: BackendKind::SystemdUserService,
            unit_name: "portagenty-wx-gy.service".into(),
            invocation_id: encode_hex(&[9, 9, 9]),
            control_group: unit_cgroup,
            mux_target: MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/tmp/x.sock"),
                session: "main".into(),
            },
            observed_at_unix_ms: 0,
            limits: SoftLimits::default(),
        };
        assert!(matches!(
            backend.reconcile(&receipt).unwrap(),
            OwnershipState::StaleBinding(_)
        ));
    }
}
