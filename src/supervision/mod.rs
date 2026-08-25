#[cfg(target_os = "linux")]
pub mod linux_systemd;
pub mod metrics;
pub mod model;
#[cfg(target_os = "linux")]
pub mod store;

use anyhow::{anyhow, Result};

#[cfg(target_os = "linux")]
pub use linux_systemd::{LinuxSystemdBackend, PendingLaunchState};
pub use model::{
    ActionKind, ActionResult, ActionStage, BackendKind, BindingReceipt, CapabilityReport,
    CapabilityState, GeneratedNames, LimitKind, LogicalSessionId, MetricKind, MetricValue,
    MuxTarget, OwnershipState, ResourceLimits, ResourceSnapshot, SupervisionMode,
};
#[cfg(target_os = "linux")]
pub use store::{PendingLaunch, ReceiptStore};

/// Capability-aware platform boundary. Implementations must revalidate a
/// binding before returning `OwnedVerified`, resource data, or performing a
/// control action.
pub trait SupervisionBackend: Send + Sync {
    fn capabilities(&self) -> CapabilityReport;

    fn reconcile(&self, receipt: &BindingReceipt) -> Result<OwnershipState>;

    fn snapshot(
        &self,
        receipt: &BindingReceipt,
        previous: Option<&ResourceSnapshot>,
    ) -> Result<ResourceSnapshot>;

    fn stop_unit(&self, receipt: &BindingReceipt) -> Result<ActionResult>;

    fn force_kill(&self, receipt: &BindingReceipt) -> Result<ActionResult>;
}

#[derive(Debug, Clone)]
pub struct UnsupportedBackend {
    reason: String,
}

impl UnsupportedBackend {
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }
}

impl SupervisionBackend for UnsupportedBackend {
    fn capabilities(&self) -> CapabilityReport {
        CapabilityReport::unsupported(self.reason.clone())
    }

    fn reconcile(&self, _receipt: &BindingReceipt) -> Result<OwnershipState> {
        Ok(OwnershipState::Unsupported(self.reason.clone()))
    }

    fn snapshot(
        &self,
        _receipt: &BindingReceipt,
        _previous: Option<&ResourceSnapshot>,
    ) -> Result<ResourceSnapshot> {
        Err(anyhow!(self.reason.clone()))
    }

    fn stop_unit(&self, _receipt: &BindingReceipt) -> Result<ActionResult> {
        Err(anyhow!(self.reason.clone()))
    }

    fn force_kill(&self, _receipt: &BindingReceipt) -> Result<ActionResult> {
        Err(anyhow!(self.reason.clone()))
    }
}

pub fn unsupported_backend() -> Box<dyn SupervisionBackend> {
    Box::new(UnsupportedBackend::new(
        "resource supervision is currently implemented only for Linux with systemd user services and cgroup v2",
    ))
}

#[cfg(target_os = "linux")]
pub fn platform_backend() -> Box<dyn SupervisionBackend> {
    match LinuxSystemdBackend::connect() {
        Ok(backend) => Box::new(backend),
        Err(error) => Box::new(UnsupportedBackend::new(format!(
            "systemd user supervision is unavailable: {error:#}"
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn platform_backend() -> Box<dyn SupervisionBackend> {
    unsupported_backend()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_backend_reports_instead_of_implying_capability() {
        let backend = UnsupportedBackend::new("missing backend");
        let report = backend.capabilities();
        assert_eq!(
            report.overall,
            CapabilityState::Unavailable("missing backend".into())
        );
    }
}
