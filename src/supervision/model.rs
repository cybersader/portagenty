use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::domain::SessionKind;

pub const LEGACY_RECEIPT_SCHEMA_VERSION: u32 = 1;
pub const RECEIPT_SCHEMA_VERSION: u32 = 2;
pub const CLAUDE_CODE_SLICE: &str = "claude-code.slice";

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
    MemoryMax,
    MemorySwapMax,
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
    pub const ALL: [Self; 5] = [
        Self::MemoryHigh,
        Self::MemoryMax,
        Self::MemorySwapMax,
        Self::CpuQuota,
        Self::TasksMax,
    ];
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub struct ResourceLimits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_high_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_swap_max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tasks_max: Option<u64>,
}

impl ResourceLimits {
    pub const GIB: u64 = 1024 * 1024 * 1024;
    pub const MIB: u64 = 1024 * 1024;

    pub fn standard_defaults() -> Self {
        Self {
            memory_high_bytes: Some(3 * Self::GIB),
            memory_max_bytes: Some(5 * Self::GIB),
            memory_swap_max_bytes: Some(512 * Self::MIB),
            cpu_quota_percent: Some(800.0),
            tasks_max: Some(1200),
        }
    }

    pub fn claude_defaults() -> Self {
        Self::standard_defaults()
    }

    pub fn defaults_for_kind(_kind: Option<SessionKind>) -> Self {
        Self::standard_defaults()
    }

    pub fn resolve_for_kind(&self, kind: Option<SessionKind>) -> Result<Self> {
        let defaults = Self::defaults_for_kind(kind);
        let resolved = Self {
            memory_high_bytes: self.memory_high_bytes.or(defaults.memory_high_bytes),
            memory_max_bytes: self.memory_max_bytes.or(defaults.memory_max_bytes),
            memory_swap_max_bytes: self
                .memory_swap_max_bytes
                .or(defaults.memory_swap_max_bytes),
            cpu_quota_percent: self.cpu_quota_percent.or(defaults.cpu_quota_percent),
            tasks_max: self.tasks_max.or(defaults.tasks_max),
        };
        resolved.validate_consistency()?;
        if kind == Some(SessionKind::ClaudeCode) {
            let defaults = Self::claude_defaults();
            for (name, actual, ceiling) in [
                (
                    "MemoryHigh",
                    resolved.memory_high_bytes,
                    defaults.memory_high_bytes,
                ),
                (
                    "MemoryMax",
                    resolved.memory_max_bytes,
                    defaults.memory_max_bytes,
                ),
                (
                    "MemorySwapMax",
                    resolved.memory_swap_max_bytes,
                    defaults.memory_swap_max_bytes,
                ),
                ("TasksMax", resolved.tasks_max, defaults.tasks_max),
            ] {
                if actual
                    .zip(ceiling)
                    .is_some_and(|(actual, ceiling)| actual > ceiling)
                {
                    return Err(anyhow!(
                        "Claude Code {name} override is weaker than the standard policy"
                    ));
                }
            }
            if resolved
                .cpu_quota_percent
                .zip(defaults.cpu_quota_percent)
                .is_some_and(|(actual, ceiling)| actual > ceiling)
            {
                return Err(anyhow!(
                    "Claude Code CPU quota override is weaker than the standard policy"
                ));
            }
        }
        Ok(resolved)
    }

    pub fn validate_consistency(&self) -> Result<()> {
        if self
            .memory_high_bytes
            .zip(self.memory_max_bytes)
            .is_some_and(|(high, max)| high > max)
        {
            return Err(anyhow!(
                "MemoryHigh must be less than or equal to MemoryMax"
            ));
        }
        if self
            .cpu_quota_percent
            .is_some_and(|value| !value.is_finite() || value <= 0.0)
        {
            return Err(anyhow!("CPU quota must be a positive finite percentage"));
        }
        if [
            self.memory_high_bytes,
            self.memory_max_bytes,
            self.memory_swap_max_bytes,
            self.tasks_max,
        ]
        .into_iter()
        .flatten()
        .any(|value| value == 0)
        {
            return Err(anyhow!("resource limits must be greater than zero"));
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.memory_high_bytes.is_none()
            && self.memory_max_bytes.is_none()
            && self.memory_swap_max_bytes.is_none()
            && self.cpu_quota_percent.is_none()
            && self.tasks_max.is_none()
    }

    pub fn is_complete(&self) -> bool {
        self.memory_high_bytes.is_some()
            && self.memory_max_bytes.is_some()
            && self.memory_swap_max_bytes.is_some()
            && self.cpu_quota_percent.is_some()
            && self.tasks_max.is_some()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "kebab-case")]
pub enum SupervisionMode {
    Normal,
    Supervised { limits: ResourceLimits },
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct WorkloadAnchorProof {
    pub protocol_version: u32,
    pub nonce: String,
    pub marker_path: PathBuf,
    pub pid: u32,
    pub start_time_ticks: u64,
}

pub(crate) fn validate_workload_marker_shape(path: &Path, nonce: &str) -> Result<()> {
    if nonce.len() != 32
        || !nonce
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(anyhow!(
            "workload nonce must be 32 lowercase hexadecimal characters"
        ));
    }
    let expected_filename = format!("{nonce}.marker.toml");
    if !path.is_absolute()
        || path.file_name().and_then(|name| name.to_str()) != Some(expected_filename.as_str())
        || path
            .parent()
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("workloads")
        || path
            .parent()
            .and_then(Path::parent)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            != Some("portagenty")
    {
        return Err(anyhow!(
            "workload marker must use the exact portagenty/workloads/<nonce>.marker.toml shape"
        ));
    }
    Ok(())
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
    pub limits: ResourceLimits,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_kind: Option<SessionKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_slice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workload_anchor: Option<WorkloadAnchorProof>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_boot_id: Option<String>,
}

impl BindingReceipt {
    pub fn is_legacy(&self) -> bool {
        self.schema_version == LEGACY_RECEIPT_SCHEMA_VERSION
    }

    pub fn validate_shape(&self) -> Result<()> {
        if !matches!(
            self.schema_version,
            LEGACY_RECEIPT_SCHEMA_VERSION | RECEIPT_SCHEMA_VERSION
        ) {
            return Err(anyhow!(
                "unsupported supervision receipt schema {}",
                self.schema_version
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
        self.limits.validate_consistency()?;
        if self.schema_version == RECEIPT_SCHEMA_VERSION {
            let anchor = self
                .workload_anchor
                .as_ref()
                .ok_or_else(|| anyhow!("current receipt is missing workload-anchor proof"))?;
            if anchor.protocol_version != 1 {
                return Err(anyhow!("invalid workload-anchor proof"));
            }
            validate_workload_marker_shape(&anchor.marker_path, &anchor.nonce)?;
            match self.session_kind {
                Some(SessionKind::ClaudeCode) => {
                    if self.requested_slice.as_deref() != Some(CLAUDE_CODE_SLICE) {
                        return Err(anyhow!(
                            "Claude Code receipt is missing its requested slice"
                        ));
                    }
                    if self.limits.resolve_for_kind(self.session_kind)? != self.limits {
                        return Err(anyhow!(
                            "Claude Code receipt contains a weak resource policy"
                        ));
                    }
                }
                _ if self.requested_slice.is_some() => {
                    return Err(anyhow!(
                        "generic receipt unexpectedly requests a Claude slice"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", content = "detail", rename_all = "kebab-case")]
pub enum OwnershipState {
    IdleSupported,
    OwnedVerified(Box<BindingReceipt>),
    LegacyRestartRequired(String),
    SplitContainment(String),
    AmbiguousBinding(String),
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
    fn supervised_defaults_are_complete_for_every_session_kind() {
        let expected = ResourceLimits {
            memory_high_bytes: Some(3 * 1024_u64.pow(3)),
            memory_max_bytes: Some(5 * 1024_u64.pow(3)),
            memory_swap_max_bytes: Some(512 * 1024_u64.pow(2)),
            cpu_quota_percent: Some(800.0),
            tasks_max: Some(1200),
        };
        assert!(ResourceLimits::default().is_empty());
        assert_eq!(ResourceLimits::defaults_for_kind(None), expected);
        assert_eq!(
            ResourceLimits::defaults_for_kind(Some(SessionKind::Shell)),
            expected
        );
        assert_eq!(
            ResourceLimits::defaults_for_kind(Some(SessionKind::ClaudeCode)),
            expected
        );
        assert!(expected.is_complete());
    }

    #[test]
    fn generic_partial_overrides_resolve_against_standard_defaults() {
        let resolved = ResourceLimits {
            memory_high_bytes: Some(2 * ResourceLimits::GIB),
            ..ResourceLimits::default()
        }
        .resolve_for_kind(Some(SessionKind::Shell))
        .unwrap();
        assert_eq!(resolved.memory_high_bytes, Some(2 * ResourceLimits::GIB));
        assert_eq!(resolved.memory_max_bytes, Some(5 * ResourceLimits::GIB));
        assert_eq!(
            resolved.memory_swap_max_bytes,
            Some(512 * ResourceLimits::MIB)
        );
        assert_eq!(resolved.cpu_quota_percent, Some(800.0));
        assert_eq!(resolved.tasks_max, Some(1200));
        assert!(resolved.is_complete());
    }

    #[test]
    fn claude_overrides_must_be_equal_or_stricter_and_consistent() {
        assert!(ResourceLimits {
            memory_high_bytes: Some(4 * ResourceLimits::GIB),
            ..ResourceLimits::default()
        }
        .resolve_for_kind(Some(SessionKind::ClaudeCode))
        .is_err());
        assert!(ResourceLimits {
            memory_high_bytes: Some(4 * ResourceLimits::GIB),
            memory_max_bytes: Some(3 * ResourceLimits::GIB),
            ..ResourceLimits::default()
        }
        .validate_consistency()
        .is_err());
        assert!(ResourceLimits {
            memory_high_bytes: Some(2 * ResourceLimits::GIB),
            memory_max_bytes: Some(4 * ResourceLimits::GIB),
            memory_swap_max_bytes: Some(256 * ResourceLimits::MIB),
            cpu_quota_percent: Some(400.0),
            tasks_max: Some(600),
        }
        .resolve_for_kind(Some(SessionKind::ClaudeCode))
        .is_ok());
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
