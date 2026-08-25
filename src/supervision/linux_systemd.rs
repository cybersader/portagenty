use std::collections::{BTreeMap, HashSet, VecDeque};
use std::ffi::OsString;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::fd::OwnedFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use crate::domain::{Session, SessionKind};
use crate::mux::{Multiplexer, TmuxAdapter};

use super::metrics::{
    cgroup_fs_path, counter_rate, cpu_percent, parse_cgroup_state, parse_cpu_stat, parse_io_stat,
    parse_keyed_u64, parse_psi, parse_single_u64,
};
use super::model::{
    ActionKind, ActionResult, ActionStage, BackendKind, BindingReceipt, CapabilityReport,
    CapabilityState, LimitKind, LogicalSessionId, MetricKind, MetricValue, MuxTarget,
    OwnershipState, ResourceLimits, ResourceSnapshot, WorkloadAnchorProof, CLAUDE_CODE_SLICE,
    LEGACY_RECEIPT_SCHEMA_VERSION, RECEIPT_SCHEMA_VERSION,
};
use super::store::{PendingLaunch, ReceiptStore};
use super::SupervisionBackend;

const SYSTEMD_DESTINATION: &str = "org.freedesktop.systemd1";
const SYSTEMD_PATH: &str = "/org/freedesktop/systemd1";
const MANAGER_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const UNIT_INTERFACE: &str = "org.freedesktop.systemd1.Unit";
const SERVICE_INTERFACE: &str = "org.freedesktop.systemd1.Service";
const SLICE_INTERFACE: &str = "org.freedesktop.systemd1.Slice";
const CGROUP_ROOT: &str = "/sys/fs/cgroup";
const START_TIMEOUT: Duration = Duration::from_secs(3);
const TARGET_TIMEOUT: Duration = Duration::from_secs(3);
const STOP_TIMEOUT_USEC: u64 = 8_000_000;
const STOP_OBSERVE_TIMEOUT: Duration = Duration::from_secs(9);
const KILL_OBSERVE_TIMEOUT: Duration = Duration::from_secs(3);
const UINT64_MAX: u64 = u64::MAX;
const ANCHOR_PROTOCOL_VERSION: u32 = 1;
const MAX_DESCENDANTS: usize = 4096;

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
    pub slice: String,
    pub memory_high: u64,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    pub cpu_quota_per_sec_usec: u64,
    pub tasks_max: u64,
    pub managed_oom_preference: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingLaunchState {
    Active(String),
    Dead(String),
    Ambiguous(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaudeSliceIdentity {
    pub control_group: String,
    pub memory_high: u64,
    pub memory_max: u64,
    pub memory_swap_max: u64,
    pub cpu_quota_per_sec_usec: u64,
    pub tasks_max: u64,
    pub managed_oom_preference: String,
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
    pub session_kind: Option<SessionKind>,
    pub requested_slice: Option<String>,
    pub executable: PathBuf,
    /// Arguments after argv[0]. The D-Bus adapter prepends the executable.
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub environment: Vec<String>,
    pub limits: ResourceLimits,
    pub pty_stdio: Option<PtyStdio>,
}

#[derive(Debug)]
pub struct PreparedLaunch {
    pub logical_id: LogicalSessionId,
    pub spec: TransientServiceSpec,
    pub mux_target: MuxTarget,
    expected_anchor: ExpectedAnchor,
    cleanup_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
struct ExpectedAnchor {
    nonce: String,
    marker_path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct WorkloadLaunchSpec {
    protocol_version: u32,
    nonce: String,
    marker_path: PathBuf,
    command: String,
    #[serde(default)]
    environment: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session_kind: Option<SessionKind>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
struct WorkloadMarker {
    protocol_version: u32,
    nonce: String,
    pid: u32,
    start_time_ticks: u64,
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
    fn claude_slice_info(&self) -> Result<Option<ClaudeSliceIdentity>>;
    fn start_transient_service(&self, spec: &TransientServiceSpec) -> Result<SystemdUnitIdentity>;
    fn unit_by_name(&self, unit_name: &str) -> Result<Option<SystemdUnitIdentity>>;
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
            slice: service
                .get_property("Slice")
                .context("reading systemd Slice")?,
            memory_high: service
                .get_property("MemoryHigh")
                .context("reading systemd MemoryHigh")?,
            memory_max: service
                .get_property("MemoryMax")
                .context("reading systemd MemoryMax")?,
            memory_swap_max: service
                .get_property("MemorySwapMax")
                .context("reading systemd MemorySwapMax")?,
            cpu_quota_per_sec_usec: service
                .get_property("CPUQuotaPerSecUSec")
                .context("reading systemd CPUQuotaPerSecUSec")?,
            tasks_max: service
                .get_property("TasksMax")
                .context("reading systemd TasksMax")?,
            managed_oom_preference: service
                .get_property("ManagedOOMPreference")
                .context("reading systemd ManagedOOMPreference")?,
        })
    }

    fn lookup_unit_by_name(&self, name: &str) -> Result<Option<SystemdUnitIdentity>> {
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

    fn claude_slice_info(&self) -> Result<Option<ClaudeSliceIdentity>> {
        let manager = self.manager_proxy()?;
        let path: OwnedObjectPath = match manager.call("GetUnit", &(CLAUDE_CODE_SLICE,)) {
            Ok(path) => path,
            Err(error) if is_no_such_unit(&error) => return Ok(None),
            Err(error) => return Err(error).context("resolving claude-code.slice"),
        };
        let slice = Proxy::new(
            &self.connection,
            SYSTEMD_DESTINATION,
            path.as_str(),
            SLICE_INTERFACE,
        )
        .context("creating systemd Claude slice proxy")?;
        Ok(Some(ClaudeSliceIdentity {
            control_group: slice
                .get_property("ControlGroup")
                .context("reading Claude slice ControlGroup")?,
            memory_high: slice
                .get_property("MemoryHigh")
                .context("reading Claude slice MemoryHigh")?,
            memory_max: slice
                .get_property("MemoryMax")
                .context("reading Claude slice MemoryMax")?,
            memory_swap_max: slice
                .get_property("MemorySwapMax")
                .context("reading Claude slice MemorySwapMax")?,
            cpu_quota_per_sec_usec: slice
                .get_property("CPUQuotaPerSecUSec")
                .context("reading Claude slice CPUQuotaPerSecUSec")?,
            tasks_max: slice
                .get_property("TasksMax")
                .context("reading Claude slice TasksMax")?,
            managed_oom_preference: slice
                .get_property("ManagedOOMPreference")
                .context("reading Claude slice ManagedOOMPreference")?,
        }))
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
            if let Some(identity) = self.lookup_unit_by_name(&spec.unit_name)? {
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

    fn unit_by_name(&self, unit_name: &str) -> Result<Option<SystemdUnitIdentity>> {
        self.lookup_unit_by_name(unit_name)
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
    if spec.session_kind == Some(SessionKind::ClaudeCode) {
        properties.push(property(
            "Slice",
            spec.requested_slice
                .clone()
                .ok_or_else(|| anyhow!("Claude Code service is missing its slice"))?,
        )?);
        properties.push(property("ManagedOOMPreference", "omit".to_string())?);
    } else if spec.requested_slice.is_some() {
        bail!("generic service unexpectedly requested a Claude slice");
    }
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
    if let Some(value) = spec.limits.memory_max_bytes {
        properties.push(property("MemoryMax", value)?);
    }
    if let Some(value) = spec.limits.memory_swap_max_bytes {
        properties.push(property("MemorySwapMax", value)?);
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

fn is_direct_claude_service_cgroup(control_group: &str) -> bool {
    Path::new(control_group)
        .parent()
        .is_some_and(|parent| parent.ends_with("claude.slice/claude-code.slice"))
}

fn current_binding_receipt(
    prepared: &PreparedLaunch,
    identity: &SystemdUnitIdentity,
    workload_anchor: WorkloadAnchorProof,
) -> Result<BindingReceipt> {
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
        session_kind: prepared.spec.session_kind,
        requested_slice: prepared.spec.requested_slice.clone(),
        workload_anchor: Some(workload_anchor),
    };
    receipt.validate_shape()?;
    Ok(receipt)
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

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn prepare_workload_anchor(
    session: &Session,
    runtime_dir: &Path,
    portagenty_executable: &Path,
) -> Result<(ExpectedAnchor, PathBuf, String)> {
    let root = runtime_dir.join("portagenty/workloads");
    ensure_private_runtime_dir(&root)?;
    let nonce = uuid::Uuid::new_v4().simple().to_string();
    let spec_path = root.join(format!("{nonce}.launch.toml"));
    let marker_path = root.join(format!("{nonce}.marker.toml"));
    let spec = WorkloadLaunchSpec {
        protocol_version: ANCHOR_PROTOCOL_VERSION,
        nonce: nonce.clone(),
        marker_path: marker_path.clone(),
        command: session.command.clone(),
        environment: session.env.clone(),
        session_kind: session.kind,
    };
    let serialized = toml::to_string(&spec).context("serializing workload launch specification")?;
    write_private_file(&spec_path, serialized.as_bytes())?;
    let command = format!(
        "exec {} __workload-anchor --spec {}",
        shell_quote(path_to_utf8(portagenty_executable, "Portagenty executable")?.as_str()),
        shell_quote(path_to_utf8(&spec_path, "workload launch specification")?.as_str())
    );
    Ok((ExpectedAnchor { nonce, marker_path }, spec_path, command))
}

fn validate_runtime_workload_path(path: &Path, nonce: &str, suffix: &str) -> Result<()> {
    validate_runtime_workload_path_in(&validated_runtime_dir()?, path, nonce, suffix)
}

fn validate_runtime_workload_path_in(
    runtime_dir: &Path,
    path: &Path,
    nonce: &str,
    suffix: &str,
) -> Result<()> {
    validate_owner_private_dir(runtime_dir, "runtime directory")?;
    let workloads = runtime_dir.join("portagenty/workloads");
    let expected = workloads.join(format!("{nonce}.{suffix}.toml"));
    if path != expected {
        bail!(
            "workload file {} is outside the exact owner runtime namespace",
            path.display()
        );
    }
    validate_owner_private_dir(
        &runtime_dir.join("portagenty"),
        "Portagenty runtime directory",
    )?;
    validate_owner_private_dir(&workloads, "workload directory")?;
    Ok(())
}

fn marker_matches_proof(marker: &WorkloadMarker, proof: &WorkloadAnchorProof) -> bool {
    marker.protocol_version == proof.protocol_version
        && marker.nonce == proof.nonce
        && marker.pid == proof.pid
        && marker.start_time_ticks == proof.start_time_ticks
}

pub(crate) fn remove_verified_workload_marker(proof: &WorkloadAnchorProof) -> Result<()> {
    remove_verified_workload_marker_in(&validated_runtime_dir()?, proof)
}

fn remove_verified_workload_marker_in(
    runtime_dir: &Path,
    proof: &WorkloadAnchorProof,
) -> Result<()> {
    super::model::validate_workload_marker_shape(&proof.marker_path, &proof.nonce)?;
    validate_runtime_workload_path_in(runtime_dir, &proof.marker_path, &proof.nonce, "marker")?;
    let marker = match read_workload_marker(&proof.marker_path) {
        Ok(marker) => marker,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok(())
        }
        Err(error) => return Err(error),
    };
    if !marker_matches_proof(&marker, proof) {
        bail!("refusing to remove a workload marker that does not match its receipt");
    }
    fs::remove_file(&proof.marker_path)
        .with_context(|| format!("removing workload marker {}", proof.marker_path.display()))
}

pub fn run_workload_anchor(spec_path: &Path) -> Result<()> {
    let filename = spec_path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| anyhow!("workload launch specification has no UTF-8 filename"))?;
    let path_nonce = filename
        .strip_suffix(".launch.toml")
        .ok_or_else(|| anyhow!("workload launch specification filename is invalid"))?;
    validate_runtime_workload_path(spec_path, path_nonce, "launch")?;
    let uid = rustix::process::geteuid().as_raw();
    let metadata = fs::symlink_metadata(spec_path).with_context(|| {
        format!(
            "reading workload launch specification {}",
            spec_path.display()
        )
    })?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
    {
        bail!(
            "workload launch specification {} is not an owner-only regular file",
            spec_path.display()
        );
    }
    let raw = fs::read_to_string(spec_path).with_context(|| {
        format!(
            "reading workload launch specification {}",
            spec_path.display()
        )
    })?;
    let spec: WorkloadLaunchSpec = toml::from_str(&raw).with_context(|| {
        format!(
            "parsing workload launch specification {}",
            spec_path.display()
        )
    })?;
    if spec.protocol_version != ANCHOR_PROTOCOL_VERSION || spec.nonce != path_nonce {
        bail!("unsupported or mismatched workload-anchor launch protocol");
    }
    super::model::validate_workload_marker_shape(&spec.marker_path, &spec.nonce)?;
    validate_runtime_workload_path(&spec.marker_path, &spec.nonce, "marker")?;
    let parent = spec
        .marker_path
        .parent()
        .ok_or_else(|| anyhow!("workload marker path has no parent"))?;
    let parent_metadata = fs::symlink_metadata(parent)
        .with_context(|| format!("reading workload marker directory {}", parent.display()))?;
    if parent_metadata.file_type().is_symlink()
        || !parent_metadata.is_dir()
        || parent_metadata.uid() != uid
        || parent_metadata.mode() & 0o777 != 0o700
    {
        bail!("workload marker directory is not owner-only");
    }
    let pid = std::process::id();
    let marker = WorkloadMarker {
        protocol_version: ANCHOR_PROTOCOL_VERSION,
        nonce: spec.nonce.clone(),
        pid,
        start_time_ticks: process_start_time_ticks(pid)?,
    };
    write_atomic_private_toml(&spec.marker_path, &marker)?;
    fs::remove_file(spec_path).with_context(|| {
        format!(
            "removing one-shot launch specification {}",
            spec_path.display()
        )
    })?;

    let mut command = if is_bare_shell_command(&spec.command) {
        std::process::Command::new(spec.command.trim())
    } else {
        let mut command = std::process::Command::new("bash");
        command.arg("-c").arg(&spec.command);
        command
    };
    command.envs(spec.environment);
    command.env("PORTAGENTY_WORKLOAD_NONCE", &spec.nonce);
    let error = command.exec();
    Err(error).context("executing anchored workload")
}

fn is_bare_shell_command(command: &str) -> bool {
    matches!(
        command.trim(),
        "bash" | "sh" | "zsh" | "fish" | "ash" | "dash"
    )
}

fn write_atomic_private_toml(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow!("private marker path has no parent"))?;
    let temp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("marker"),
        uuid::Uuid::new_v4().simple()
    ));
    let bytes = toml::to_string(value).context("serializing workload marker")?;
    write_private_file(&temp, bytes.as_bytes())?;
    fs::rename(&temp, path)
        .with_context(|| format!("publishing workload marker {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting workload marker permissions {}", path.display()))?;
    Ok(())
}

pub struct LinuxSystemdBackend {
    api: Arc<dyn SystemdApi>,
    cgroup_root: PathBuf,
    workload_executable: PathBuf,
}

impl LinuxSystemdBackend {
    pub fn connect() -> Result<Self> {
        Self::connect_with_workload_executable(
            std::env::current_exe().context("resolving the Portagenty executable")?,
        )
    }

    #[doc(hidden)]
    pub fn connect_with_workload_executable(workload_executable: PathBuf) -> Result<Self> {
        Ok(Self {
            api: Arc::new(DbusSystemdApi::connect()?),
            cgroup_root: PathBuf::from(CGROUP_ROOT),
            workload_executable,
        })
    }

    #[cfg(test)]
    pub fn with_api(api: Arc<dyn SystemdApi>, cgroup_root: PathBuf) -> Self {
        Self {
            api,
            cgroup_root,
            workload_executable: std::env::current_exe().unwrap(),
        }
    }

    pub fn prepare_tmux_launch(
        &self,
        logical_id: LogicalSessionId,
        session: &Session,
        limits: ResourceLimits,
    ) -> Result<PreparedLaunch> {
        let limits = limits.resolve_for_kind(session.kind)?;
        self.require_limit_capabilities(&limits)?;
        self.preflight_session_policy(session.kind)?;
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
        let (expected_anchor, launch_spec, anchor_command) =
            prepare_workload_anchor(session, &runtime_dir, &self.workload_executable)?;
        let (server_environment, pane_environment) =
            supervised_tmux_environments(&session.env, &runtime_dir)?;
        let args = adapter.create_detached_args_with_command_and_environment(
            session,
            &names.tmux_session,
            &anchor_command,
            &pane_environment,
        )?;
        let tmux_args = os_args_to_utf8(args)?;
        let tmux_executable = resolve_executable("tmux")?;
        let mut args = vec![
            "-u".into(),
            "DBUS_SESSION_BUS_ADDRESS".into(),
            "-u".into(),
            "XDG_RUNTIME_DIR".into(),
            path_to_utf8(&tmux_executable, "tmux executable")?,
        ];
        args.extend(tmux_args);
        let executable = resolve_executable("env")?;
        Ok(PreparedLaunch {
            logical_id,
            mux_target: MuxTarget::TmuxPrivate {
                socket: names.tmux_socket,
                session: names.tmux_session,
            },
            expected_anchor,
            cleanup_paths: vec![launch_spec],
            spec: TransientServiceSpec {
                unit_name: names.unit_name,
                session_kind: session.kind,
                requested_slice: (session.kind == Some(SessionKind::ClaudeCode))
                    .then(|| CLAUDE_CODE_SLICE.to_string()),
                executable,
                args,
                working_directory: session.cwd.clone(),
                environment: server_environment,
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
        limits: ResourceLimits,
    ) -> Result<PreparedLaunch> {
        let limits = limits.resolve_for_kind(session.kind)?;
        self.require_limit_capabilities(&limits)?;
        self.preflight_session_policy(session.kind)?;
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
        let (expected_anchor, launch_spec, anchor_command) =
            prepare_workload_anchor(session, &runtime_dir, &self.workload_executable)?;
        write_private_file(
            &layout_path,
            crate::mux::zellij::render_layout_with_tab_name_and_command(
                session,
                &tab_name,
                &anchor_command,
            )
            .as_bytes(),
        )?;

        Ok(PreparedLaunch {
            logical_id,
            mux_target: MuxTarget::Zellij {
                session: names.zellij_session.clone(),
                runtime_dir: Some(runtime_dir),
            },
            expected_anchor,
            cleanup_paths: vec![layout_path, launch_spec],
            spec: TransientServiceSpec {
                unit_name: names.unit_name,
                session_kind: session.kind,
                requested_slice: (session.kind == Some(SessionKind::ClaudeCode))
                    .then(|| CLAUDE_CODE_SLICE.to_string()),
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
            self.verify_service_policy(&identity, &prepared.spec)?;
            wait_for_mux_target(&prepared.mux_target, TARGET_TIMEOUT)?;
            let workload_anchor = wait_for_workload_anchor(
                &prepared.expected_anchor,
                &identity.control_group,
                &prepared.mux_target,
                TARGET_TIMEOUT,
            )?;
            current_binding_receipt(prepared, &identity, workload_anchor)
        })();
        if launch_result.is_err() {
            let _ = self.api.stop_unit(&prepared.spec.unit_name);
        }
        launch_result.context("validating the new supervised workload")
    }

    pub fn create_tmux_binding(
        &self,
        store: &ReceiptStore,
        logical_id: LogicalSessionId,
        session: &Session,
        limits: ResourceLimits,
    ) -> Result<BindingReceipt> {
        if let Some(existing) = store.find(&logical_id)? {
            return match self.reconcile(&existing)? {
                OwnershipState::OwnedVerified(_) => Ok(existing),
                state => bail!("an incompatible supervision receipt already exists: {state:?}"),
            };
        }
        if let Some(pending) = store.find_pending(&logical_id)? {
            bail!("a pending supervision launch already exists: {pending:?}");
        }
        let prepared = self.prepare_tmux_launch(logical_id.clone(), session, limits)?;
        self.start_and_persist(store, &prepared)
            .context("creating supervised tmux binding")
    }

    pub fn create_zellij_binding(
        &self,
        store: &ReceiptStore,
        logical_id: LogicalSessionId,
        workspace_name: &str,
        session: &Session,
        limits: ResourceLimits,
    ) -> Result<BindingReceipt> {
        if let Some(existing) = store.find(&logical_id)? {
            return match self.reconcile(&existing)? {
                OwnershipState::OwnedVerified(_) => Ok(existing),
                state => bail!("an incompatible supervision receipt already exists: {state:?}"),
            };
        }
        if let Some(pending) = store.find_pending(&logical_id)? {
            bail!("a pending supervision launch already exists: {pending:?}");
        }
        let prepared =
            self.prepare_zellij_launch(logical_id.clone(), workspace_name, session, limits)?;
        self.start_and_persist(store, &prepared)
            .context("creating supervised Zellij binding")
    }

    fn start_and_persist(
        &self,
        store: &ReceiptStore,
        prepared: &PreparedLaunch,
    ) -> Result<BindingReceipt> {
        let creator_pid = std::process::id();
        let pending = PendingLaunch {
            logical_id: prepared.logical_id.clone(),
            unit_name: prepared.spec.unit_name.clone(),
            mux_target: prepared.mux_target.clone(),
            marker_path: prepared.expected_anchor.marker_path.clone(),
            created_at_unix_ms: now_unix_ms(),
            creator_pid,
            creator_start_time_ticks: process_start_time_ticks(creator_pid)
                .context("recording pending-launch creator process proof")?,
            last_error: None,
        };
        store.begin_pending(pending)?;
        match self.start_prepared(prepared) {
            Ok(receipt) => {
                if let Err(error) = store.finalize_pending(receipt.clone()) {
                    let _ = self.api.stop_unit(&receipt.unit_name);
                    let _ = store.mark_pending_error(
                        &receipt.logical_id,
                        format!("receipt persistence failed: {error:#}"),
                    );
                    return Err(error).context("persisting supervision receipt");
                }
                Ok(receipt)
            }
            Err(error) => {
                // Creation may already have started. Request only the normal
                // non-force stop, then retain the pending journal unless every
                // unit/target/marker probe proves the partial launch absent.
                let _ = self.api.stop_unit(&prepared.spec.unit_name);
                let unit_present = self
                    .api
                    .unit_by_name(&prepared.spec.unit_name)
                    .unwrap_or(Some(SystemdUnitIdentity {
                        unit_name: prepared.spec.unit_name.clone(),
                        invocation_id: Vec::new(),
                        control_group: String::new(),
                        active_state: String::new(),
                        sub_state: String::new(),
                        transient: true,
                        slice: String::new(),
                        memory_high: UINT64_MAX,
                        memory_max: UINT64_MAX,
                        memory_swap_max: UINT64_MAX,
                        cpu_quota_per_sec_usec: UINT64_MAX,
                        tasks_max: UINT64_MAX,
                        managed_oom_preference: String::new(),
                    }))
                    .is_some();
                let target_present = mux_target_exists(&prepared.mux_target).unwrap_or(true);
                let marker_present = prepared.expected_anchor.marker_path.exists();
                if !unit_present && !target_present && !marker_present {
                    let _ = store.clear_pending(&prepared.logical_id);
                } else {
                    let _ = store.mark_pending_error(
                        &prepared.logical_id,
                        format!("post-creation validation failed: {error:#}"),
                    );
                }
                Err(error)
            }
        }
    }

    pub fn reconcile_pending(&self, pending: &PendingLaunch) -> Result<PendingLaunchState> {
        pending.validate_shape()?;
        if !pending.has_creator_proof() {
            return Ok(PendingLaunchState::Ambiguous(
                "pending launch has no creator process proof".into(),
            ));
        }
        let creator_alive = match process_start_time_ticks(pending.creator_pid) {
            Ok(start_time) => start_time == pending.creator_start_time_ticks,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                false
            }
            Err(error) => {
                return Ok(PendingLaunchState::Ambiguous(format!(
                    "pending creator process could not be verified: {error:#}"
                )))
            }
        };
        let unit_present = self.api.unit_by_name(&pending.unit_name)?.is_some();
        let target_present = mux_target_exists(&pending.mux_target)?;
        let marker_present = pending
            .marker_path
            .try_exists()
            .with_context(|| format!("probing {}", pending.marker_path.display()))?;
        Ok(classify_pending_launch(
            creator_alive,
            unit_present,
            target_present,
            marker_present,
        ))
    }

    pub fn remove_dead_pending(
        &self,
        store: &ReceiptStore,
        expected: &PendingLaunch,
    ) -> Result<()> {
        store.update_locked(|file| {
            let index = file
                .pending_launches
                .iter()
                .position(|pending| pending.logical_id == expected.logical_id)
                .ok_or_else(|| anyhow!("the pending launch is no longer present"))?;
            let current = &file.pending_launches[index];
            if current != expected {
                bail!("the pending launch changed after confirmation; refresh and retry");
            }
            match self.reconcile_pending(current)? {
                PendingLaunchState::Dead(_) => {
                    file.pending_launches.remove(index);
                    Ok(())
                }
                PendingLaunchState::Active(reason) => {
                    bail!("pending launch creator is still active ({reason}); nothing was removed")
                }
                PendingLaunchState::Ambiguous(reason) => {
                    bail!("pending launch remains ambiguous ({reason}); nothing was removed")
                }
            }
        })
    }

    /// Remove one exact stale receipt without signalling any process. The
    /// receipt store stays exclusively locked while both runtime identities
    /// are revalidated, preventing a concurrent launch from being mistaken for
    /// the dead generation the user confirmed.
    pub fn remove_stale_binding(
        &self,
        store: &ReceiptStore,
        expected: &BindingReceipt,
    ) -> Result<()> {
        self.remove_stale_binding_with_probe(store, expected, mux_target_exists)
    }

    fn remove_stale_binding_with_probe(
        &self,
        store: &ReceiptStore,
        expected: &BindingReceipt,
        target_exists: impl Fn(&MuxTarget) -> Result<bool>,
    ) -> Result<()> {
        store.update_locked(|file| {
            let index = file
                .bindings
                .iter()
                .position(|receipt| receipt.logical_id == expected.logical_id)
                .ok_or_else(|| anyhow!("the stale receipt is no longer present"))?;
            let current = &file.bindings[index];
            if current != expected {
                bail!("the ownership receipt changed after confirmation; refresh and retry");
            }
            if self.identity_if_present(current)?.is_some() {
                bail!("the exact systemd invocation is still present; no receipt was removed");
            }
            if target_exists(&current.mux_target)? {
                bail!("the exact multiplexer target is still present; no receipt was removed");
            }
            if let Some(anchor) = current.workload_anchor.as_ref() {
                remove_verified_workload_marker(anchor)?;
            }
            file.bindings.remove(index);
            Ok(())
        })
    }

    fn preflight_session_policy(&self, kind: Option<SessionKind>) -> Result<()> {
        if kind != Some(SessionKind::ClaudeCode) {
            return Ok(());
        }
        let slice = self.api.claude_slice_info()?.ok_or_else(|| {
            anyhow!("claude-code.slice is not loaded in the systemd user manager")
        })?;
        if !slice
            .control_group
            .ends_with("/claude.slice/claude-code.slice")
        {
            bail!(
                "claude-code.slice reported unexpected placement {:?}",
                slice.control_group
            );
        }
        for (name, value) in [
            ("MemoryHigh", slice.memory_high),
            ("MemoryMax", slice.memory_max),
            ("MemorySwapMax", slice.memory_swap_max),
            ("CPUQuotaPerSecUSec", slice.cpu_quota_per_sec_usec),
        ] {
            if value == 0 || value == UINT64_MAX {
                bail!("claude-code.slice has no finite positive {name}");
            }
        }
        if slice.memory_high > slice.memory_max {
            bail!("claude-code.slice MemoryHigh exceeds MemoryMax");
        }
        if slice.managed_oom_preference != "omit" {
            bail!("claude-code.slice ManagedOOMPreference is not omit");
        }
        Ok(())
    }

    fn verify_service_policy(
        &self,
        identity: &SystemdUnitIdentity,
        spec: &TransientServiceSpec,
    ) -> Result<()> {
        if spec
            .requested_slice
            .as_deref()
            .is_some_and(|requested| identity.slice != requested)
        {
            bail!("systemd placed the service in an unexpected slice");
        }
        if spec.session_kind == Some(SessionKind::ClaudeCode)
            && !is_direct_claude_service_cgroup(&identity.control_group)
        {
            bail!("Claude Code service is not directly beneath claude-code.slice");
        }
        for (name, expected, actual) in [
            (
                "MemoryHigh",
                spec.limits.memory_high_bytes,
                identity.memory_high,
            ),
            (
                "MemoryMax",
                spec.limits.memory_max_bytes,
                identity.memory_max,
            ),
            (
                "MemorySwapMax",
                spec.limits.memory_swap_max_bytes,
                identity.memory_swap_max,
            ),
            ("TasksMax", spec.limits.tasks_max, identity.tasks_max),
        ] {
            if expected.is_some_and(|expected| actual != expected) {
                bail!("systemd read-back for {name} does not match the requested value");
            }
        }
        if let Some(percent) = spec.limits.cpu_quota_percent {
            let expected = (percent * 10_000.0).round() as u64;
            if identity.cpu_quota_per_sec_usec != expected {
                bail!("systemd CPU quota read-back does not match the requested value");
            }
        }
        if spec.session_kind == Some(SessionKind::ClaudeCode)
            && identity.managed_oom_preference != "omit"
        {
            bail!("Claude Code service ManagedOOMPreference is not omit");
        }
        Ok(())
    }

    fn require_limit_capabilities(&self, limits: &ResourceLimits) -> Result<()> {
        if limits.is_empty() {
            return Ok(());
        }
        let report = self.capabilities();
        for (requested, kind) in [
            (limits.memory_high_bytes.is_some(), LimitKind::MemoryHigh),
            (limits.memory_max_bytes.is_some(), LimitKind::MemoryMax),
            (
                limits.memory_swap_max_bytes.is_some(),
                LimitKind::MemorySwapMax,
            ),
            (limits.cpu_quota_percent.is_some(), LimitKind::CpuQuota),
            (limits.tasks_max.is_some(), LimitKind::TasksMax),
        ] {
            if requested && report.limits.get(&kind) != Some(&CapabilityState::Supported) {
                bail!("requested {kind:?} resource limit is not supported on this host");
            }
        }
        Ok(())
    }

    fn verified_binding(&self, receipt: &BindingReceipt) -> Result<SystemdUnitIdentity> {
        if receipt.schema_version == LEGACY_RECEIPT_SCHEMA_VERSION {
            bail!("legacy v1 receipt is attach-only and requires restart for resource ownership");
        }
        let identity = self.verified_identity(receipt)?;
        if !mux_target_exists(&receipt.mux_target)? {
            bail!("recorded multiplexer target is no longer present");
        }
        let anchor = receipt
            .workload_anchor
            .as_ref()
            .ok_or_else(|| anyhow!("current receipt is missing workload-anchor proof"))?;
        match verify_workload_anchor(anchor, &receipt.control_group, &receipt.mux_target)? {
            ContainmentStatus::Verified => Ok(identity),
            ContainmentStatus::Split(reason) => bail!("split containment: {reason}"),
        }
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
        if receipt.schema_version == RECEIPT_SCHEMA_VERSION {
            if let Some(requested_slice) = &receipt.requested_slice {
                if identity.slice != *requested_slice {
                    bail!("systemd slice no longer matches the receipt");
                }
            }
            for (name, expected, actual) in [
                (
                    "MemoryHigh",
                    receipt.limits.memory_high_bytes,
                    identity.memory_high,
                ),
                (
                    "MemoryMax",
                    receipt.limits.memory_max_bytes,
                    identity.memory_max,
                ),
                (
                    "MemorySwapMax",
                    receipt.limits.memory_swap_max_bytes,
                    identity.memory_swap_max,
                ),
                ("TasksMax", receipt.limits.tasks_max, identity.tasks_max),
            ] {
                if expected.is_some_and(|expected| expected != actual) {
                    bail!("systemd {name} no longer matches the receipt");
                }
            }
            if let Some(percent) = receipt.limits.cpu_quota_percent {
                let expected = (percent * 10_000.0).round() as u64;
                if identity.cpu_quota_per_sec_usec != expected {
                    bail!("systemd CPU quota no longer matches the receipt");
                }
            }
            if receipt.session_kind == Some(SessionKind::ClaudeCode)
                && !is_direct_claude_service_cgroup(&identity.control_group)
            {
                bail!("Claude Code service is no longer directly beneath claude-code.slice");
            }
            if receipt.session_kind == Some(SessionKind::ClaudeCode)
                && identity.managed_oom_preference != "omit"
            {
                bail!("Claude Code service no longer has ManagedOOMPreference=omit");
            }
        }
        Ok(Some(identity))
    }

    fn reconcile_legacy(&self, receipt: &BindingReceipt) -> OwnershipState {
        let unit_present = match self.identity_if_present(receipt) {
            Ok(unit) => unit.is_some(),
            Err(error) => {
                return OwnershipState::AmbiguousBinding(format!(
                    "legacy unit identity could not be verified: {error:#}"
                ))
            }
        };
        let target_present = match mux_target_exists(&receipt.mux_target) {
            Ok(present) => present,
            Err(error) => {
                return OwnershipState::AmbiguousBinding(format!(
                    "legacy multiplexer target could not be probed: {error:#}"
                ))
            }
        };
        match (unit_present, target_present) {
            (true, true) => OwnershipState::LegacyRestartRequired(
                "v1 service remains attachable but has no workload-anchor proof; exit and relaunch to gain resource ownership".into(),
            ),
            (false, false) => OwnershipState::StaleBinding(
                "legacy service and private multiplexer target are both absent".into(),
            ),
            _ => OwnershipState::AmbiguousBinding(
                "legacy unit and private multiplexer target are only partially present".into(),
            ),
        }
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
        if receipt.is_legacy() {
            return Ok(self.reconcile_legacy(receipt));
        }
        let unit_present = match self.identity_if_present(receipt) {
            Ok(unit) => unit.is_some(),
            Err(error) => {
                return Ok(OwnershipState::AmbiguousBinding(format!(
                    "unit identity could not be verified: {error:#}"
                )))
            }
        };
        let target_present = match mux_target_exists(&receipt.mux_target) {
            Ok(present) => present,
            Err(error) => {
                return Ok(OwnershipState::AmbiguousBinding(format!(
                    "multiplexer target could not be probed: {error:#}"
                )))
            }
        };
        match (unit_present, target_present) {
            (false, false) => Ok(OwnershipState::StaleBinding(
                "service and private multiplexer target are both absent".into(),
            )),
            (true, true) => {
                let anchor = receipt
                    .workload_anchor
                    .as_ref()
                    .ok_or_else(|| anyhow!("current receipt is missing workload-anchor proof"))?;
                match verify_workload_anchor(anchor, &receipt.control_group, &receipt.mux_target) {
                    Ok(ContainmentStatus::Verified) => {
                        Ok(OwnershipState::OwnedVerified(Box::new(receipt.clone())))
                    }
                    Ok(ContainmentStatus::Split(reason)) => {
                        Ok(OwnershipState::SplitContainment(reason))
                    }
                    Err(error) => Ok(OwnershipState::AmbiguousBinding(format!(
                        "workload-anchor proof could not be verified: {error:#}"
                    ))),
                }
            }
            _ => Ok(OwnershipState::AmbiguousBinding(
                "service and private multiplexer target are only partially present".into(),
            )),
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
        let invocation_id = decode_hex(&receipt.invocation_id)?;
        if self.identity_if_present(receipt)?.is_none() {
            return Ok(ActionResult {
                logical_id: receipt.logical_id.clone(),
                requested: ActionStage::SystemdStop,
                attempted: Vec::new(),
                completed: true,
                final_state: "verified systemd invocation was already inactive".into(),
            });
        }
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
        self.verified_binding(receipt)?;
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
    limits.insert(
        LimitKind::MemoryMax,
        controller_state(&controllers, "memory"),
    );
    limits.insert(
        LimitKind::MemorySwapMax,
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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ContainmentStatus {
    Verified,
    Split(String),
}

fn wait_for_workload_anchor(
    expected: &ExpectedAnchor,
    control_group: &str,
    mux_target: &MuxTarget,
    timeout: Duration,
) -> Result<WorkloadAnchorProof> {
    super::model::validate_workload_marker_shape(&expected.marker_path, &expected.nonce)?;
    validate_runtime_workload_path(&expected.marker_path, &expected.nonce, "marker")?;
    let deadline = Instant::now() + timeout;
    loop {
        match read_workload_marker(&expected.marker_path) {
            Ok(marker) => {
                if marker.nonce != expected.nonce {
                    bail!("workload marker nonce does not match the launch specification");
                }
                let proof = WorkloadAnchorProof {
                    protocol_version: marker.protocol_version,
                    nonce: marker.nonce,
                    marker_path: expected.marker_path.clone(),
                    pid: marker.pid,
                    start_time_ticks: marker.start_time_ticks,
                };
                match verify_workload_anchor(&proof, control_group, mux_target)? {
                    ContainmentStatus::Verified => return Ok(proof),
                    ContainmentStatus::Split(reason) => bail!("{reason}"),
                }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) if !expected.marker_path.exists() => {
                let _ = error;
            }
            Err(error) => return Err(error),
        }
        if Instant::now() >= deadline {
            bail!("workload anchor did not appear within {timeout:?}");
        }
        thread::sleep(Duration::from_millis(25));
    }
}

fn read_workload_marker(path: &Path) -> Result<WorkloadMarker> {
    let uid = rustix::process::geteuid().as_raw();
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o600
    {
        bail!("workload marker is not an owner-only regular file");
    }
    let marker: WorkloadMarker =
        toml::from_str(&fs::read_to_string(path)?).context("parsing workload marker")?;
    if marker.protocol_version != ANCHOR_PROTOCOL_VERSION || marker.nonce.len() < 32 {
        bail!("unsupported workload marker protocol");
    }
    Ok(marker)
}

fn process_environment_value<'a>(environment: &'a [u8], key: &str) -> Option<&'a [u8]> {
    let prefix = format!("{key}=");
    environment
        .split(|byte| *byte == 0)
        .find_map(|entry| entry.strip_prefix(prefix.as_bytes()))
}

fn verify_mux_anchor_with(
    proof: &WorkloadAnchorProof,
    mux_target: &MuxTarget,
    environment: &[u8],
    tmux_pane_pid: impl FnOnce(&Path, &str) -> Result<u32>,
) -> Result<()> {
    match mux_target {
        MuxTarget::TmuxPrivate { socket, session } => {
            let pane_pid = tmux_pane_pid(socket, session)?;
            if pane_pid != proof.pid {
                bail!(
                    "workload root PID {} does not match exact tmux pane PID {pane_pid}",
                    proof.pid
                );
            }
        }
        MuxTarget::TmuxShared { .. } => {
            bail!("current workload proof cannot bind to a shared tmux target")
        }
        MuxTarget::Zellij {
            session,
            runtime_dir: Some(runtime_dir),
        } => {
            if process_environment_value(environment, "ZELLIJ_SESSION_NAME")
                != Some(session.as_bytes())
            {
                bail!("workload root is not bound to the exact Zellij session");
            }
            if process_environment_value(environment, "XDG_RUNTIME_DIR")
                != Some(runtime_dir.as_os_str().as_encoded_bytes())
            {
                bail!("workload root is not bound to the exact Zellij runtime directory");
            }
        }
        MuxTarget::Zellij {
            runtime_dir: None, ..
        } => bail!("current workload proof requires an exact Zellij runtime directory"),
    }
    Ok(())
}

fn verify_workload_anchor(
    proof: &WorkloadAnchorProof,
    control_group: &str,
    mux_target: &MuxTarget,
) -> Result<ContainmentStatus> {
    super::model::validate_workload_marker_shape(&proof.marker_path, &proof.nonce)?;
    validate_runtime_workload_path(&proof.marker_path, &proof.nonce, "marker")?;
    let marker = read_workload_marker(&proof.marker_path)?;
    if !marker_matches_proof(&marker, proof) {
        bail!("workload marker no longer matches the receipt");
    }
    if process_start_time_ticks(proof.pid)? != proof.start_time_ticks {
        bail!("workload root PID was reused");
    }
    let environment = fs::read(format!("/proc/{}/environ", proof.pid))?;
    let expected_nonce = format!("PORTAGENTY_WORKLOAD_NONCE={}", proof.nonce);
    if !environment
        .split(|byte| *byte == 0)
        .any(|entry| entry == expected_nonce.as_bytes())
    {
        bail!("workload root nonce is absent from /proc");
    }
    verify_mux_anchor_with(proof, mux_target, &environment, |socket, session| {
        TmuxAdapter::with_socket(socket).pane_pid(session)
    })?;
    let root_cgroup = process_cgroup(proof.pid)?;
    let descendants = bounded_descendants(proof.pid)?;
    let mut descendant_cgroups = Vec::new();
    for pid in descendants {
        let cgroup = match process_cgroup(pid) {
            Ok(cgroup) => cgroup,
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        };
        descendant_cgroups.push((pid, cgroup));
    }
    classify_containment_paths(control_group, &root_cgroup, &descendant_cgroups)
}

fn classify_pending_launch(
    creator_alive: bool,
    unit_present: bool,
    target_present: bool,
    marker_present: bool,
) -> PendingLaunchState {
    let evidence = format!(
        "creator_alive={creator_alive}, unit_present={unit_present}, target_present={target_present}, marker_present={marker_present}"
    );
    if creator_alive {
        PendingLaunchState::Active(evidence)
    } else if !unit_present && !target_present && !marker_present {
        PendingLaunchState::Dead(evidence)
    } else {
        PendingLaunchState::Ambiguous(evidence)
    }
}

fn classify_containment_paths(
    control_group: &str,
    root_cgroup: &str,
    descendants: &[(u32, String)],
) -> Result<ContainmentStatus> {
    if root_cgroup != control_group {
        return Ok(ContainmentStatus::Split(format!(
            "workload root escaped the receipted service cgroup into {root_cgroup}"
        )));
    }
    for (pid, cgroup) in descendants {
        if cgroup != control_group {
            let label = if cgroup.contains("/background.slice/") {
                "intentional external background scope"
            } else {
                "escaped descendant"
            };
            return Ok(ContainmentStatus::Split(format!(
                "{label} PID {pid} is in {cgroup}; the complete workload is not owned"
            )));
        }
    }
    Ok(ContainmentStatus::Verified)
}

fn process_start_time_ticks(pid: u32) -> Result<u64> {
    let raw = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let close = raw
        .rfind(')')
        .ok_or_else(|| anyhow!("malformed /proc/{pid}/stat"))?;
    raw[close + 1..]
        .split_whitespace()
        .nth(19)
        .ok_or_else(|| anyhow!("missing start time in /proc/{pid}/stat"))?
        .parse()
        .with_context(|| format!("parsing /proc/{pid}/stat start time"))
}

fn process_cgroup(pid: u32) -> Result<String> {
    let raw = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    let mut unified = raw.lines().filter_map(|line| {
        let mut parts = line.splitn(3, ':');
        let hierarchy = parts.next()?;
        let controllers = parts.next()?;
        let path = parts.next()?;
        (hierarchy == "0" && controllers.is_empty()).then(|| path.to_string())
    });
    let path = unified
        .next()
        .ok_or_else(|| anyhow!("PID {pid} has no unified cgroup-v2 entry"))?;
    if unified.next().is_some() {
        bail!("PID {pid} has multiple unified cgroup-v2 entries");
    }
    super::model::validate_control_group(&path)?;
    Ok(path)
}

fn bounded_descendants(root: u32) -> Result<Vec<u32>> {
    bounded_descendants_in(Path::new("/proc"), root)
}

fn bounded_descendants_in(proc_root: &Path, root: u32) -> Result<Vec<u32>> {
    let mut queue = VecDeque::from([root]);
    let mut seen = HashSet::from([root]);
    let mut descendants = Vec::new();
    while let Some(pid) = queue.pop_front() {
        let task_dir = proc_root.join(pid.to_string()).join("task");
        let entries = match fs::read_dir(&task_dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && pid != root => continue,
            Err(error) => {
                return Err(error).with_context(|| format!("reading {}", task_dir.display()))
            }
        };
        let mut tids = 0usize;
        for entry in entries {
            let entry = entry.with_context(|| format!("reading {}", task_dir.display()))?;
            let Some(tid) = entry
                .file_name()
                .to_str()
                .and_then(|tid| tid.parse::<u32>().ok())
            else {
                continue;
            };
            tids += 1;
            if tids > MAX_DESCENDANTS {
                bail!("workload thread walk exceeded {MAX_DESCENDANTS} threads for PID {pid}");
            }
            let children_path = task_dir.join(tid.to_string()).join("children");
            let raw = match fs::read_to_string(&children_path) {
                Ok(raw) => raw,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("reading {}", children_path.display()))
                }
            };
            for child in raw.split_whitespace() {
                let child: u32 = child
                    .parse()
                    .with_context(|| format!("parsing child PID in {}", children_path.display()))?;
                if seen.insert(child) {
                    if seen.len() > MAX_DESCENDANTS {
                        bail!("workload descendant walk exceeded {MAX_DESCENDANTS} processes");
                    }
                    descendants.push(child);
                    queue.push_back(child);
                }
            }
        }
    }
    Ok(descendants)
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

fn validate_owner_private_dir(path: &Path, label: &str) -> Result<()> {
    let uid = rustix::process::geteuid().as_raw();
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("reading {label} {}", path.display()))?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != uid
        || metadata.mode() & 0o777 != 0o700
    {
        bail!("{label} is not an owner-only regular directory");
    }
    Ok(())
}

fn create_owner_private_dir(path: &Path) -> Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).with_context(|| {
                format!("setting private runtime permissions on {}", path.display())
            })?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error).with_context(|| format!("creating {}", path.display())),
    }
    validate_owner_private_dir(path, "runtime subdirectory")
}

fn ensure_private_runtime_dir(path: &Path) -> Result<()> {
    let runtime_dir = validated_runtime_dir()?;
    let portagenty = runtime_dir.join("portagenty");
    let allowed = [
        portagenty.join("tmux"),
        portagenty.join("zellij"),
        portagenty.join("workloads"),
    ];
    if !allowed.iter().any(|candidate| candidate == path) {
        bail!("private runtime directory is outside the exact Portagenty namespace");
    }
    create_owner_private_dir(&portagenty)?;
    create_owner_private_dir(path)
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

fn supervised_tmux_environments(
    overrides: &BTreeMap<String, String>,
    runtime_dir: &Path,
) -> Result<(Vec<String>, BTreeMap<String, String>)> {
    let runtime = path_to_utf8(runtime_dir, "runtime directory")?;
    let mut pane_environment = overrides.clone();
    pane_environment.insert("XDG_RUNTIME_DIR".into(), runtime.clone());
    pane_environment.insert(
        "DBUS_SESSION_BUS_ADDRESS".into(),
        format!("unix:path={runtime}/bus"),
    );

    let mut server_environment = sanitized_environment(overrides)?;
    server_environment.retain(|entry| {
        !entry.starts_with("XDG_RUNTIME_DIR=") && !entry.starts_with("DBUS_SESSION_BUS_ADDRESS=")
    });
    Ok((server_environment, pane_environment))
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

    #[test]
    fn descendant_walk_reads_children_from_every_thread() {
        let temp = tempfile::tempdir().unwrap();
        for (pid, tids) in [(10, vec![10, 11]), (20, vec![20]), (30, vec![30])] {
            for tid in tids {
                fs::create_dir_all(temp.path().join(format!("{pid}/task/{tid}"))).unwrap();
                fs::write(temp.path().join(format!("{pid}/task/{tid}/children")), "").unwrap();
            }
        }
        fs::write(temp.path().join("10/task/11/children"), "20").unwrap();
        fs::write(temp.path().join("20/task/20/children"), "30").unwrap();

        assert_eq!(
            bounded_descendants_in(temp.path(), 10).unwrap(),
            vec![20, 30]
        );
    }

    #[test]
    fn mux_anchor_proof_binds_to_exact_tmux_and_zellij_targets() {
        let proof = WorkloadAnchorProof {
            protocol_version: ANCHOR_PROTOCOL_VERSION,
            nonce: "0123456789abcdef0123456789abcdef".into(),
            marker_path: PathBuf::from(
                "/run/user/1000/portagenty/workloads/0123456789abcdef0123456789abcdef.marker.toml",
            ),
            pid: 123,
            start_time_ticks: 456,
        };
        let socket = PathBuf::from("/run/user/1000/portagenty/test/tmux.sock");
        let tmux = MuxTarget::TmuxPrivate {
            socket: socket.clone(),
            session: "exact-session".into(),
        };
        verify_mux_anchor_with(&proof, &tmux, b"", |actual_socket, actual_session| {
            assert_eq!(actual_socket, socket);
            assert_eq!(actual_session, "exact-session");
            Ok(123)
        })
        .unwrap();
        assert!(verify_mux_anchor_with(&proof, &tmux, b"", |_, _| Ok(999)).is_err());

        let zellij = MuxTarget::Zellij {
            session: "exact-zellij".into(),
            runtime_dir: Some(PathBuf::from("/run/user/1000/portagenty/zellij")),
        };
        let environment =
            b"ZELLIJ_SESSION_NAME=exact-zellij\0XDG_RUNTIME_DIR=/run/user/1000/portagenty/zellij\0";
        verify_mux_anchor_with(&proof, &zellij, environment, |_, _| Ok(0)).unwrap();
        let replaced =
            b"ZELLIJ_SESSION_NAME=replaced\0XDG_RUNTIME_DIR=/run/user/1000/portagenty/zellij\0";
        assert!(verify_mux_anchor_with(&proof, &zellij, replaced, |_, _| Ok(0)).is_err());
    }

    #[test]
    fn supervised_tmux_hides_user_bus_from_server_and_restores_it_to_pane() {
        let mut overrides = BTreeMap::new();
        overrides.insert("DBUS_SESSION_BUS_ADDRESS".into(), "malicious".into());
        overrides.insert("XDG_RUNTIME_DIR".into(), "/tmp/malicious".into());
        overrides.insert("CUSTOM".into(), "kept".into());
        let runtime = Path::new("/run/user/1000");
        let (server, pane) = supervised_tmux_environments(&overrides, runtime).unwrap();
        assert!(!server.iter().any(|entry| {
            entry.starts_with("DBUS_SESSION_BUS_ADDRESS=") || entry.starts_with("XDG_RUNTIME_DIR=")
        }));
        assert!(server.iter().any(|entry| entry == "CUSTOM=kept"));
        assert_eq!(
            pane.get("XDG_RUNTIME_DIR").map(String::as_str),
            Some("/run/user/1000")
        );
        assert_eq!(
            pane.get("DBUS_SESSION_BUS_ADDRESS").map(String::as_str),
            Some("unix:path=/run/user/1000/bus")
        );
    }

    struct FakeSystemd {
        manager: ManagerInfo,
        claude_slice: Option<ClaudeSliceIdentity>,
        unit: Mutex<Option<SystemdUnitIdentity>>,
        stops: Mutex<Vec<String>>,
        kills: Mutex<Vec<String>>,
    }

    impl SystemdApi for FakeSystemd {
        fn manager_info(&self) -> Result<ManagerInfo> {
            Ok(self.manager.clone())
        }

        fn claude_slice_info(&self) -> Result<Option<ClaudeSliceIdentity>> {
            Ok(self.claude_slice.clone())
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

        fn unit_by_name(&self, unit_name: &str) -> Result<Option<SystemdUnitIdentity>> {
            Ok(self
                .unit
                .lock()
                .unwrap()
                .clone()
                .filter(|unit| unit.unit_name == unit_name))
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

    fn fake_unit(
        unit_name: impl Into<String>,
        invocation_id: Vec<u8>,
        control_group: impl Into<String>,
    ) -> SystemdUnitIdentity {
        SystemdUnitIdentity {
            unit_name: unit_name.into(),
            invocation_id,
            control_group: control_group.into(),
            active_state: "active".into(),
            sub_state: "running".into(),
            transient: true,
            slice: "app.slice".into(),
            memory_high: UINT64_MAX,
            memory_max: UINT64_MAX,
            memory_swap_max: UINT64_MAX,
            cpu_quota_per_sec_usec: UINT64_MAX,
            tasks_max: UINT64_MAX,
            managed_oom_preference: "none".into(),
        }
    }

    fn standard_claude_slice(manager_control_group: &str) -> ClaudeSliceIdentity {
        ClaudeSliceIdentity {
            control_group: format!("{manager_control_group}/claude.slice/claude-code.slice"),
            memory_high: 8 * ResourceLimits::GIB,
            memory_max: 10 * ResourceLimits::GIB,
            memory_swap_max: ResourceLimits::GIB,
            cpu_quota_per_sec_usec: 16 * 1_000_000,
            tasks_max: 4096,
            managed_oom_preference: "omit".into(),
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
    fn new_binding_receipts_use_v2_workload_evidence() {
        let logical_id =
            LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "shell").unwrap();
        let nonce = "0123456789abcdef0123456789abcdef".to_string();
        let marker_path = PathBuf::from(format!("/tmp/portagenty/workloads/{nonce}.marker.toml"));
        let prepared = PreparedLaunch {
            logical_id: logical_id.clone(),
            spec: TransientServiceSpec {
                unit_name: "portagenty-wtest.service".into(),
                session_kind: Some(SessionKind::Shell),
                requested_slice: None,
                executable: PathBuf::from("/usr/bin/tmux"),
                args: vec!["new-session".into()],
                working_directory: PathBuf::from("/tmp"),
                environment: Vec::new(),
                limits: ResourceLimits::default(),
                pty_stdio: None,
            },
            mux_target: MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/tmp/portagenty-test.sock"),
                session: "main".into(),
            },
            expected_anchor: ExpectedAnchor {
                nonce: nonce.clone(),
                marker_path: marker_path.clone(),
            },
            cleanup_paths: Vec::new(),
        };
        let identity = fake_unit(
            "portagenty-wtest.service",
            vec![1; 16],
            "/user.slice/user-1000.slice/user@1000.service/app.slice/portagenty-wtest.service",
        );
        let receipt = current_binding_receipt(
            &prepared,
            &identity,
            WorkloadAnchorProof {
                protocol_version: ANCHOR_PROTOCOL_VERSION,
                nonce,
                marker_path,
                pid: 123,
                start_time_ticks: 456,
            },
        )
        .unwrap();

        assert_eq!(receipt.schema_version, RECEIPT_SCHEMA_VERSION);
        assert_eq!(receipt.logical_id, logical_id);
        assert!(receipt.workload_anchor.is_some());
    }

    fn stale_receipt() -> BindingReceipt {
        BindingReceipt {
            schema_version: LEGACY_RECEIPT_SCHEMA_VERSION,
            logical_id: LogicalSessionId::new(
                "550e8400-e29b-41d4-a716-446655440000",
                "shell",
            )
            .unwrap(),
            backend: BackendKind::SystemdUserService,
            unit_name: "portagenty-w550e8400e29b41d4a716446655440000-g00112233445566778899aabbccddeeff.service".into(),
            invocation_id: "00112233445566778899aabbccddeeff".into(),
            control_group: "/user.slice/user-1000.slice/user@1000.service/app.slice/portagenty-test.service".into(),
            mux_target: MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/run/user/1000/portagenty/test/tmux.sock"),
                session: "opaque-target".into(),
            },
            observed_at_unix_ms: 1,
            limits: ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
        }
    }

    fn fake_backend(unit: Option<SystemdUnitIdentity>) -> LinuxSystemdBackend {
        LinuxSystemdBackend::with_api(
            Arc::new(FakeSystemd {
                manager: ManagerInfo {
                    version: "259".into(),
                    control_group: "/user.slice/user-1000.slice/user@1000.service".into(),
                },
                claude_slice: None,
                unit: Mutex::new(unit),
                stops: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
            }),
            PathBuf::from("/sys/fs/cgroup"),
        )
    }

    fn fake_backend_with_slice(slice: ClaudeSliceIdentity) -> LinuxSystemdBackend {
        LinuxSystemdBackend::with_api(
            Arc::new(FakeSystemd {
                manager: ManagerInfo {
                    version: "259".into(),
                    control_group: "/user.slice/user-1000.slice/user@1000.service".into(),
                },
                claude_slice: Some(slice),
                unit: Mutex::new(None),
                stops: Mutex::new(Vec::new()),
                kills: Mutex::new(Vec::new()),
            }),
            PathBuf::from("/sys/fs/cgroup"),
        )
    }

    #[test]
    fn service_properties_include_all_resource_limits_and_safe_stop_policy() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            session_kind: Some(SessionKind::ClaudeCode),
            requested_slice: Some(CLAUDE_CODE_SLICE.into()),
            executable: PathBuf::from("/usr/bin/tmux"),
            args: vec!["new-session".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: vec!["PATH=/usr/bin".into()],
            limits: ResourceLimits {
                memory_high_bytes: Some(1024),
                memory_max_bytes: Some(2048),
                memory_swap_max_bytes: Some(512),
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
            "Slice",
            "ManagedOOMPreference",
            "MemoryHigh",
            "MemoryMax",
            "MemorySwapMax",
            "CPUQuotaPerSecUSec",
            "TasksMax",
        ] {
            assert!(names.contains(required), "missing {required}");
        }
    }

    #[test]
    fn generic_service_omits_claude_only_properties() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            session_kind: Some(SessionKind::Shell),
            requested_slice: None,
            executable: PathBuf::from("/usr/bin/tmux"),
            args: vec!["new-session".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: Vec::new(),
            limits: ResourceLimits {
                memory_high_bytes: Some(1024),
                memory_max_bytes: Some(2048),
                memory_swap_max_bytes: Some(512),
                cpu_quota_percent: Some(100.0),
                tasks_max: Some(50),
            },
            pty_stdio: None,
        };
        let properties = service_properties(&spec).unwrap();
        let names: std::collections::HashSet<&str> =
            properties.iter().map(|(name, _)| name.as_str()).collect();

        assert!(!names.contains("Slice"));
        assert!(!names.contains("ManagedOOMPreference"));
        for resource_limit in [
            "MemoryHigh",
            "MemoryMax",
            "MemorySwapMax",
            "CPUQuotaPerSecUSec",
            "TasksMax",
        ] {
            assert!(names.contains(resource_limit), "missing {resource_limit}");
        }
    }

    #[test]
    fn claude_slice_preflight_rejects_weak_or_malformed_aggregate_policy() {
        let manager = "/user.slice/user-1000.slice/user@1000.service";
        let healthy = standard_claude_slice(manager);
        fake_backend_with_slice(healthy.clone())
            .preflight_session_policy(Some(SessionKind::ClaudeCode))
            .unwrap();

        let mut unlimited = healthy.clone();
        unlimited.memory_max = UINT64_MAX;
        assert!(fake_backend_with_slice(unlimited)
            .preflight_session_policy(Some(SessionKind::ClaudeCode))
            .is_err());

        let mut inconsistent = healthy.clone();
        inconsistent.memory_high = inconsistent.memory_max + 1;
        assert!(fake_backend_with_slice(inconsistent)
            .preflight_session_policy(Some(SessionKind::ClaudeCode))
            .is_err());

        let mut wrong_hierarchy = healthy.clone();
        wrong_hierarchy.control_group = format!("{manager}/app.slice/claude-code.slice");
        assert!(fake_backend_with_slice(wrong_hierarchy)
            .preflight_session_policy(Some(SessionKind::ClaudeCode))
            .is_err());

        let mut wrong_oomd = healthy;
        wrong_oomd.managed_oom_preference = "none".into();
        assert!(fake_backend_with_slice(wrong_oomd)
            .preflight_session_policy(Some(SessionKind::ClaudeCode))
            .is_err());
    }

    #[test]
    fn service_policy_readback_rejects_wrong_limit_or_nested_slice_placement() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            session_kind: Some(SessionKind::ClaudeCode),
            requested_slice: Some(CLAUDE_CODE_SLICE.into()),
            executable: PathBuf::from("/usr/bin/tmux"),
            args: vec!["new-session".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: Vec::new(),
            limits: ResourceLimits::claude_defaults(),
            pty_stdio: None,
        };
        let mut identity = fake_unit(
            spec.unit_name.clone(),
            vec![1; 16],
            "/user.slice/user-1000.slice/user@1000.service/claude.slice/claude-code.slice/portagenty-wx-gy.service",
        );
        identity.slice = CLAUDE_CODE_SLICE.into();
        identity.memory_high = 3 * ResourceLimits::GIB;
        identity.memory_max = 5 * ResourceLimits::GIB;
        identity.memory_swap_max = 512 * ResourceLimits::MIB;
        identity.cpu_quota_per_sec_usec = 8_000_000;
        identity.tasks_max = 1200;
        identity.managed_oom_preference = "omit".into();
        fake_backend(None)
            .verify_service_policy(&identity, &spec)
            .unwrap();

        identity.memory_max += 1;
        assert!(fake_backend(None)
            .verify_service_policy(&identity, &spec)
            .is_err());
        identity.memory_max = 5 * ResourceLimits::GIB;
        identity.control_group = "/user.slice/user-1000.slice/user@1000.service/claude.slice/claude-code.slice/nested.slice/portagenty-wx-gy.service".into();
        assert!(fake_backend(None)
            .verify_service_policy(&identity, &spec)
            .is_err());
    }

    #[test]
    fn pty_stdio_becomes_controlling_terminal_properties() {
        let spec = TransientServiceSpec {
            unit_name: "portagenty-wx-gy.service".into(),
            session_kind: None,
            requested_slice: None,
            executable: PathBuf::from("/usr/bin/zellij"),
            args: vec!["--session".into(), "test".into()],
            working_directory: PathBuf::from("/tmp"),
            environment: vec!["PATH=/usr/bin".into()],
            limits: ResourceLimits::default(),
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
            claude_slice: None,
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
            claude_slice: None,
            unit: Mutex::new(Some(fake_unit(
                "portagenty-wx-gy.service",
                vec![1, 2, 3],
                unit_cgroup.clone(),
            ))),
            stops: Mutex::new(Vec::new()),
            kills: Mutex::new(Vec::new()),
        });
        let backend = LinuxSystemdBackend::with_api(api, temp.path().to_path_buf());
        let receipt = BindingReceipt {
            schema_version: LEGACY_RECEIPT_SCHEMA_VERSION,
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
            limits: ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
        };
        assert!(matches!(
            backend.reconcile(&receipt).unwrap(),
            OwnershipState::AmbiguousBinding(_)
        ));
    }

    #[test]
    fn pending_is_dead_only_when_creator_and_all_artifacts_are_absent() {
        assert!(matches!(
            classify_pending_launch(false, false, false, false),
            PendingLaunchState::Dead(_)
        ));
        assert!(matches!(
            classify_pending_launch(true, false, false, false),
            PendingLaunchState::Active(_)
        ));
        for evidence in [
            (false, true, false, false),
            (false, false, true, false),
            (false, false, false, true),
            (false, true, true, true),
        ] {
            assert!(matches!(
                classify_pending_launch(evidence.0, evidence.1, evidence.2, evidence.3),
                PendingLaunchState::Ambiguous(_)
            ));
        }
    }

    #[test]
    fn non_force_stop_treats_an_absent_exact_invocation_as_complete() {
        let receipt = stale_receipt();
        let backend = fake_backend(None);
        let result = backend.stop_unit(&receipt).unwrap();
        assert!(result.completed);
        assert!(result.attempted.is_empty());
    }

    #[test]
    fn stale_cleanup_removes_only_the_exact_dead_receipt() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let receipt = stale_receipt();
        store.upsert(receipt.clone()).unwrap();

        fake_backend(None)
            .remove_stale_binding_with_probe(&store, &receipt, |_| Ok(false))
            .unwrap();

        assert!(store.find(&receipt.logical_id).unwrap().is_none());
    }

    #[test]
    fn verified_marker_cleanup_requires_exact_path_and_content() {
        let temp = tempfile::tempdir().unwrap();
        fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let workloads = temp.path().join("portagenty/workloads");
        fs::create_dir_all(&workloads).unwrap();
        fs::set_permissions(
            temp.path().join("portagenty"),
            fs::Permissions::from_mode(0o700),
        )
        .unwrap();
        fs::set_permissions(&workloads, fs::Permissions::from_mode(0o700)).unwrap();
        let nonce = "0123456789abcdef0123456789abcdef";
        let marker_path = workloads.join(format!("{nonce}.marker.toml"));
        let proof = WorkloadAnchorProof {
            protocol_version: ANCHOR_PROTOCOL_VERSION,
            nonce: nonce.into(),
            marker_path: marker_path.clone(),
            pid: 123,
            start_time_ticks: 456,
        };
        write_private_file(
            &marker_path,
            toml::to_string(&WorkloadMarker {
                protocol_version: ANCHOR_PROTOCOL_VERSION,
                nonce: nonce.into(),
                pid: 999,
                start_time_ticks: 456,
            })
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        assert!(remove_verified_workload_marker_in(temp.path(), &proof).is_err());
        assert!(marker_path.exists());

        fs::remove_file(&marker_path).unwrap();
        write_private_file(
            &marker_path,
            toml::to_string(&WorkloadMarker {
                protocol_version: ANCHOR_PROTOCOL_VERSION,
                nonce: nonce.into(),
                pid: 123,
                start_time_ticks: 456,
            })
            .unwrap()
            .as_bytes(),
        )
        .unwrap();
        remove_verified_workload_marker_in(temp.path(), &proof).unwrap();
        assert!(!marker_path.exists());
    }

    #[test]
    fn stale_cleanup_refuses_a_replaced_receipt_generation() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let expected = stale_receipt();
        let mut replacement = expected.clone();
        replacement.invocation_id = "ffeeddccbbaa99887766554433221100".into();
        replacement.unit_name = "portagenty-w550e8400e29b41d4a716446655440000-gffeeddccbbaa99887766554433221100.service".into();
        store.upsert(replacement.clone()).unwrap();

        let error = fake_backend(None)
            .remove_stale_binding_with_probe(&store, &expected, |_| Ok(false))
            .unwrap_err();

        assert!(format!("{error:#}").contains("changed after confirmation"));
        assert_eq!(store.find(&expected.logical_id).unwrap(), Some(replacement));
    }

    #[test]
    fn stale_cleanup_refuses_when_exact_invocation_is_present() {
        let receipt = stale_receipt();
        let temp = tempfile::tempdir().unwrap();
        let cgroup_root = temp.path().join("cgroup");
        let cgroup_path = cgroup_root.join(receipt.control_group.trim_start_matches('/'));
        fs::create_dir_all(&cgroup_path).unwrap();
        let api = Arc::new(FakeSystemd {
            manager: ManagerInfo {
                version: "259".into(),
                control_group: "/user.slice/user-1000.slice/user@1000.service".into(),
            },
            claude_slice: None,
            unit: Mutex::new(Some(fake_unit(
                receipt.unit_name.clone(),
                decode_hex(&receipt.invocation_id).unwrap(),
                receipt.control_group.clone(),
            ))),
            stops: Mutex::new(Vec::new()),
            kills: Mutex::new(Vec::new()),
        });
        let backend = LinuxSystemdBackend::with_api(api, cgroup_root);
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        store.upsert(receipt.clone()).unwrap();

        let error = backend
            .remove_stale_binding_with_probe(&store, &receipt, |_| Ok(false))
            .unwrap_err();

        assert!(format!("{error:#}").contains("still present"));
        assert!(store.find(&receipt.logical_id).unwrap().is_some());
    }

    #[test]
    fn containment_classifier_rejects_escaped_root_and_background_descendant() {
        let expected = "/user.slice/u/app.slice/example.service";
        assert!(matches!(
            classify_containment_paths(
                expected,
                "/user.slice/u/background.slice/run.scope",
                &[],
            )
            .unwrap(),
            ContainmentStatus::Split(reason) if reason.contains("workload root escaped")
        ));
        assert!(matches!(
            classify_containment_paths(
                expected,
                expected,
                &[(42, "/user.slice/u/background.slice/build.scope".into())],
            )
            .unwrap(),
            ContainmentStatus::Split(reason)
                if reason.contains("intentional external background scope")
                    && reason.contains("not owned")
        ));
        assert_eq!(
            classify_containment_paths(expected, expected, &[(42, expected.into())]).unwrap(),
            ContainmentStatus::Verified
        );
    }

    #[test]
    fn current_process_proc_identity_is_parseable_without_global_scan() {
        let pid = std::process::id();
        assert!(process_start_time_ticks(pid).unwrap() > 0);
        assert!(process_cgroup(pid).unwrap().starts_with('/'));
        assert!(!bounded_descendants(pid).unwrap().contains(&pid));
    }

    #[test]
    fn stale_cleanup_refuses_when_exact_mux_target_is_present() {
        let temp = tempfile::tempdir().unwrap();
        let store = ReceiptStore::new(temp.path().join("state/supervision.toml"));
        let receipt = stale_receipt();
        store.upsert(receipt.clone()).unwrap();

        let error = fake_backend(None)
            .remove_stale_binding_with_probe(&store, &receipt, |_| Ok(true))
            .unwrap_err();

        assert!(format!("{error:#}").contains("multiplexer target is still present"));
        assert!(store.find(&receipt.logical_id).unwrap().is_some());
    }
}
