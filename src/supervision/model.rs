use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub const RECEIPT_SCHEMA_VERSION: u32 = 1;

/// Stable, user-facing identity for one declared session.
///
/// Resource ownership is never inferred from a PID, cwd, multiplexer display
/// name, or cgroup path. Those values are observations attached to this ID.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct LogicalSessionId {
    pub workspace_id: String,
    pub session_name: String,
}

impl LogicalSessionId {
    pub fn new(workspace_id: impl Into<String>, session_name: impl Into<String>) -> Result<Self> {
        let workspace_id = workspace_id.into();
        let session_name = session_name.into();
        Uuid::parse_str(&workspace_id)
            .map_err(|_| anyhow!("workspace id {workspace_id:?} is not a valid UUID"))?;
        if session_name.trim().is_empty() {
            return Err(anyhow!("session name cannot be empty"));
        }
        Ok(Self {
            workspace_id,
            session_name,
        })
    }

    pub fn workspace_uuid(&self) -> Result<Uuid> {
        Uuid::parse_str(&self.workspace_id)
            .map_err(|_| anyhow!("workspace id {:?} is not a valid UUID", self.workspace_id))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    SystemdUserService,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "reason", rename_all = "kebab-case")]
pub enum CapabilityState {
    Supported,
    Unavailable(String),
    NotImplemented,
    Unsupported,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MetricKind {
    CpuUsage,
    CpuRate,
    MemoryCurrent,
    MemoryPeak,
    MemoryEvents,
    SwapCurrent,
    SwapPeak,
    SwapEvents,
    TasksCurrent,
    TasksPeak,
    TasksEvents,
    IoTotals,
    IoRates,
    CpuPressure,
    MemoryPressure,
    IoPressure,
    CgroupState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionKind {
    GracefulStop,
    StopUnit,
    ForceKill,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LimitKind {
    MemoryHigh,
    CpuQuota,
    TasksMax,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CapabilityReport {
    pub backend: BackendKind,
    pub overall: CapabilityState,
    #[serde(default)]
    pub metrics: BTreeMap<MetricKind, CapabilityState>,
    #[serde(default)]
    pub actions: BTreeMap<ActionKind, CapabilityState>,
    #[serde(default)]
    pub limits: BTreeMap<LimitKind, CapabilityState>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

impl CapabilityReport {
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            backend: BackendKind::Unsupported,
            overall: CapabilityState::Unavailable(reason.into()),
            metrics: MetricKind::ALL
                .into_iter()
                .map(|kind| (kind, CapabilityState::Unsupported))
                .collect(),
            actions: ActionKind::ALL
                .into_iter()
                .map(|kind| (kind, CapabilityState::Unsupported))
                .collect(),
            limits: LimitKind::ALL
                .into_iter()
                .map(|kind| (kind, CapabilityState::Unsupported))
                .collect(),
            notes: Vec::new(),
        }
    }
}

impl MetricKind {
    pub const ALL: [Self; 17] = [
        Self::CpuUsage,
        Self::CpuRate,
        Self::MemoryCurrent,
        Self::MemoryPeak,
        Self::MemoryEvents,
        Self::SwapCurrent,
        Self::SwapPeak,
        Self::SwapEvents,
        Self::TasksCurrent,
        Self::TasksPeak,
        Self::TasksEvents,
        Self::IoTotals,
        Self::IoRates,
        Self::CpuPressure,
        Self::MemoryPressure,
        Self::IoPressure,
        Self::CgroupState,
    ];
}

impl ActionKind {
    pub const ALL: [Self; 3] = [Self::GracefulStop, Self::StopUnit, Self::ForceKill];
}

impl LimitKind {
    pub const ALL: [Self; 3] = [Self::MemoryHigh, Self::CpuQuota, Self::TasksMax];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct SoftLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_max: Option<u64>,
}

impl SoftLimits {
    pub fn is_empty(&self) -> bool {
        self.memory_high_bytes.is_none()
            && self.cpu_quota_percent.is_none()
            && self.tasks_max.is_none()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum SupervisionMode {
    Normal,
    Supervised { limits: SoftLimits },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum MuxTarget {
    TmuxPrivate {
        socket: PathBuf,
        session: String,
    },
    TmuxShared {
        session: String,
    },
    Zellij {
        session: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        runtime_dir: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct BindingReceipt {
    pub schema_version: u32,
    pub logical_id: LogicalSessionId,
    pub backend: BackendKind,
    pub unit_name: String,
    pub invocation_id: String,
    pub control_group: String,
    pub mux_target: MuxTarget,
    pub observed_at_unix_ms: u64,
    #[serde(default)]
    pub limits: SoftLimits,
}

impl BindingReceipt {
    pub fn validate_shape(&self) -> Result<()> {
        if self.schema_version != RECEIPT_SCHEMA_VERSION {
            return Err(anyhow!(
                "unsupported supervision receipt schema {} (expected {})",
                self.schema_version,
                RECEIPT_SCHEMA_VERSION
            ));
        }
        self.logical_id.workspace_uuid()?;
        if !self.unit_name.starts_with("portagenty-w") || !self.unit_name.ends_with(".service") {
            return Err(anyhow!("invalid Portagenty unit name {:?}", self.unit_name));
        }
        if self.invocation_id.is_empty() {
            return Err(anyhow!("supervision receipt has an empty invocation id"));
        }
        validate_control_group(&self.control_group)?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "kebab-case")]
pub enum OwnershipState {
    IdleSupported,
    OwnedVerified(Box<BindingReceipt>),
    ExistingUnverified,
    Unmanaged,
    StaleBinding(String),
    Unsupported(String),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "value", rename_all = "kebab-case")]
pub enum MetricValue<T> {
    Value(T),
    Unsupported,
    Unavailable(String),
    Error(String),
}

impl<T> MetricValue<T> {
    pub fn as_ref(&self) -> MetricValue<&T> {
        match self {
            Self::Value(value) => MetricValue::Value(value),
            Self::Unsupported => MetricValue::Unsupported,
            Self::Unavailable(reason) => MetricValue::Unavailable(reason.clone()),
            Self::Error(reason) => MetricValue::Error(reason.clone()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CpuStat {
    pub usage_usec: u64,
    pub user_usec: Option<u64>,
    pub system_usec: Option<u64>,
    #[serde(default)]
    pub extra: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct IoTotals {
    pub read_bytes: u64,
    pub write_bytes: u64,
    pub read_ios: u64,
    pub write_ios: u64,
    pub discard_bytes: u64,
    pub discard_ios: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct PsiLine {
    pub avg10: f64,
    pub avg60: f64,
    pub avg300: f64,
    pub total_usec: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct PsiSnapshot {
    pub some: Option<PsiLine>,
    pub full: Option<PsiLine>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct CgroupState {
    pub populated: Option<bool>,
    pub frozen: Option<bool>,
    #[serde(default)]
    pub extra: BTreeMap<String, u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ResourceSnapshot {
    pub captured_at_unix_ms: u64,
    pub cpu: MetricValue<CpuStat>,
    pub cpu_percent: MetricValue<f64>,
    pub memory_current_bytes: MetricValue<u64>,
    pub memory_peak_bytes: MetricValue<u64>,
    pub memory_events: MetricValue<BTreeMap<String, u64>>,
    pub swap_current_bytes: MetricValue<u64>,
    pub swap_peak_bytes: MetricValue<u64>,
    pub swap_events: MetricValue<BTreeMap<String, u64>>,
    pub tasks_current: MetricValue<u64>,
    pub tasks_peak: MetricValue<u64>,
    pub tasks_events: MetricValue<BTreeMap<String, u64>>,
    pub io_totals: MetricValue<IoTotals>,
    pub io_read_bytes_per_sec: MetricValue<f64>,
    pub io_write_bytes_per_sec: MetricValue<f64>,
    pub cpu_pressure: MetricValue<PsiSnapshot>,
    pub memory_pressure: MetricValue<PsiSnapshot>,
    pub io_pressure: MetricValue<PsiSnapshot>,
    pub cgroup_state: MetricValue<CgroupState>,
}

impl ResourceSnapshot {
    pub fn unavailable(captured_at_unix_ms: u64, reason: impl Into<String>) -> Self {
        let reason = reason.into();
        Self {
            captured_at_unix_ms,
            cpu: MetricValue::Unavailable(reason.clone()),
            cpu_percent: MetricValue::Unavailable(reason.clone()),
            memory_current_bytes: MetricValue::Unavailable(reason.clone()),
            memory_peak_bytes: MetricValue::Unavailable(reason.clone()),
            memory_events: MetricValue::Unavailable(reason.clone()),
            swap_current_bytes: MetricValue::Unavailable(reason.clone()),
            swap_peak_bytes: MetricValue::Unavailable(reason.clone()),
            swap_events: MetricValue::Unavailable(reason.clone()),
            tasks_current: MetricValue::Unavailable(reason.clone()),
            tasks_peak: MetricValue::Unavailable(reason.clone()),
            tasks_events: MetricValue::Unavailable(reason.clone()),
            io_totals: MetricValue::Unavailable(reason.clone()),
            io_read_bytes_per_sec: MetricValue::Unavailable(reason.clone()),
            io_write_bytes_per_sec: MetricValue::Unavailable(reason.clone()),
            cpu_pressure: MetricValue::Unavailable(reason.clone()),
            memory_pressure: MetricValue::Unavailable(reason.clone()),
            io_pressure: MetricValue::Unavailable(reason.clone()),
            cgroup_state: MetricValue::Unavailable(reason),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ActionStage {
    BackendGraceful,
    SystemdStop,
    ForceKill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ActionResult {
    pub logical_id: LogicalSessionId,
    pub requested: ActionStage,
    #[serde(default)]
    pub attempted: Vec<ActionStage>,
    pub completed: bool,
    pub final_state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GeneratedNames {
    pub unit_name: String,
    pub tmux_socket: PathBuf,
    pub tmux_session: String,
    pub zellij_session: String,
}

pub fn generate_names(
    logical_id: &LogicalSessionId,
    generation: Uuid,
    runtime_dir: &Path,
) -> Result<GeneratedNames> {
    let workspace = logical_id.workspace_uuid()?.simple().to_string();
    let generation = generation.simple().to_string();
    let unit_name = format!("portagenty-w{workspace}-g{generation}.service");
    let zellij_session = format!("pa-w{}-g{}", &workspace[..8], &generation[..16]);
    if unit_name.len() > 255 {
        return Err(anyhow!("generated systemd unit name is too long"));
    }
    let runtime_root = runtime_dir.join("portagenty");
    Ok(GeneratedNames {
        unit_name,
        tmux_socket: runtime_root
            .join("tmux")
            .join(format!("w{workspace}-g{generation}.sock")),
        tmux_session: "main".to_string(),
        zellij_session,
    })
}

pub fn validate_control_group(value: &str) -> Result<()> {
    if !value.starts_with('/') {
        return Err(anyhow!("control group path must be absolute: {value:?}"));
    }
    for component in Path::new(value).components() {
        if matches!(component, std::path::Component::ParentDir) {
            return Err(anyhow!(
                "control group path contains parent traversal: {value:?}"
            ));
        }
    }
    Ok(())
}

pub fn parse_memory_size(raw: &str) -> Result<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(anyhow!("memory size cannot be empty"));
    }
    let split_at = raw.find(|c: char| !c.is_ascii_digit()).unwrap_or(raw.len());
    let (digits, suffix) = raw.split_at(split_at);
    let value: u64 = digits
        .parse()
        .map_err(|_| anyhow!("invalid memory size {raw:?}"))?;
    if value == 0 {
        return Err(anyhow!("memory size must be greater than zero"));
    }
    let multiplier = match suffix.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => 1024,
        "m" | "mb" | "mib" => 1024_u64.pow(2),
        "g" | "gb" | "gib" => 1024_u64.pow(3),
        "t" | "tb" | "tib" => 1024_u64.pow(4),
        _ => return Err(anyhow!("unsupported memory-size suffix in {raw:?}")),
    };
    value
        .checked_mul(multiplier)
        .ok_or_else(|| anyhow!("memory size {raw:?} overflows u64"))
}

pub fn parse_cpu_quota(raw: &str) -> Result<f64> {
    let value: f64 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid CPU quota {raw:?}"))?;
    if !value.is_finite() || value <= 0.0 {
        return Err(anyhow!("CPU quota must be a positive finite percentage"));
    }
    Ok(value)
}

pub fn parse_tasks_max(raw: &str) -> Result<u64> {
    let value: u64 = raw
        .trim()
        .parse()
        .map_err(|_| anyhow!("invalid TasksMax value {raw:?}"))?;
    if value == 0 {
        return Err(anyhow!("TasksMax must be greater than zero"));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn logical() -> LogicalSessionId {
        LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "private name").unwrap()
    }

    #[test]
    fn logical_identity_requires_uuid_and_session_name() {
        assert!(LogicalSessionId::new("not-a-uuid", "shell").is_err());
        assert!(LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", " ").is_err());
    }

    #[test]
    fn generated_names_are_opaque_and_deterministic_for_generation() {
        let generation = Uuid::parse_str("67e55044-10b1-426f-9247-bb680e5fe0c8").unwrap();
        let names = generate_names(&logical(), generation, Path::new("/run/user/1000")).unwrap();
        assert!(names.unit_name.ends_with(".service"));
        assert!(!names.unit_name.contains("private"));
        assert!(!names.tmux_socket.to_string_lossy().contains("private"));
        assert!(!names.zellij_session.contains("private"));
        assert!(names.zellij_session.len() <= 32);
        assert_eq!(names.tmux_session, "main");
    }

    #[test]
    fn memory_sizes_use_binary_multipliers() {
        assert_eq!(parse_memory_size("12G").unwrap(), 12 * 1024_u64.pow(3));
        assert_eq!(parse_memory_size("512MiB").unwrap(), 512 * 1024_u64.pow(2));
        assert!(parse_memory_size("0").is_err());
        assert!(parse_memory_size("12watts").is_err());
    }

    #[test]
    fn quota_and_task_parsers_reject_nonpositive_values() {
        assert_eq!(parse_cpu_quota("300").unwrap(), 300.0);
        assert!(parse_cpu_quota("0").is_err());
        assert!(parse_cpu_quota("NaN").is_err());
        assert_eq!(parse_tasks_max("1200").unwrap(), 1200);
        assert!(parse_tasks_max("0").is_err());
    }

    #[test]
    fn control_group_requires_absolute_non_traversing_path() {
        assert!(validate_control_group("/user.slice/example.service").is_ok());
        assert!(validate_control_group("relative/path").is_err());
        assert!(validate_control_group("/user.slice/../system.slice").is_err());
    }

    #[test]
    fn unsupported_report_names_every_capability() {
        let report = CapabilityReport::unsupported("not Linux");
        assert_eq!(report.metrics.len(), MetricKind::ALL.len());
        assert_eq!(report.actions.len(), ActionKind::ALL.len());
        assert_eq!(report.limits.len(), LimitKind::ALL.len());
    }
}
