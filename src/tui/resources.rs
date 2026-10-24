use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::supervision::{
    BindingReceipt, LogicalSessionId, OwnershipState, ResourceSnapshot, SupervisionBackend,
};

const REQUEST_CAPACITY: usize = 8;
const RESULT_CAPACITY: usize = 8;
const SAMPLE_TIMEOUT: Duration = Duration::from_secs(5);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(50);

type Sampler =
    Arc<dyn Fn(BindingReceipt, Option<ResourceSnapshot>) -> SampleResult + Send + Sync + 'static>;

struct SampleRequest {
    receipt: BindingReceipt,
    previous: Option<ResourceSnapshot>,
    cancelled: Arc<AtomicBool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleCompletion {
    Completed,
    TimedOut,
    Cancelled,
}

#[derive(Debug)]
pub struct SampleResult {
    pub logical_id: LogicalSessionId,
    pub ownership: OwnershipState,
    pub snapshot: Option<ResourceSnapshot>,
    pub error: Option<String>,
    pub completion: SampleCompletion,
}

#[derive(Debug)]
pub struct SampleCancellation {
    cancelled: Arc<AtomicBool>,
}

impl SampleCancellation {
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

/// One bounded coordinator for the lifetime of a session-list TUI. Each D-Bus
/// and cgroup sample runs on its own operation thread so a wedged backend call
/// cannot block rendering, cancellation, or later refreshes. Timed-out operation
/// threads are detached; their result channel is dropped, so a late completion
/// is ignored and the process can still exit normally.
pub struct ResourceWorker {
    requests: Option<SyncSender<SampleRequest>>,
    results: Option<Receiver<SampleResult>>,
    cancellations: Mutex<Vec<Weak<AtomicBool>>>,
    thread: Option<JoinHandle<()>>,
}

fn reconciliation_failure(logical_id: LogicalSessionId, error: &anyhow::Error) -> SampleResult {
    let error = format!("{error:#}");
    SampleResult {
        logical_id,
        ownership: OwnershipState::AmbiguousBinding(error.clone()),
        snapshot: None,
        error: Some(error),
        completion: SampleCompletion::Completed,
    }
}

fn interrupted_result(
    logical_id: LogicalSessionId,
    completion: SampleCompletion,
    reason: String,
) -> SampleResult {
    SampleResult {
        logical_id,
        ownership: OwnershipState::AmbiguousBinding(reason.clone()),
        snapshot: None,
        error: Some(reason),
        completion,
    }
}

fn sample_receipt(receipt: BindingReceipt, previous: Option<ResourceSnapshot>) -> SampleResult {
    let logical_id = receipt.logical_id.clone();
    match crate::supervision::LinuxSystemdBackend::connect() {
        Ok(backend) => match backend.reconcile(&receipt) {
            Ok(ownership @ OwnershipState::OwnedVerified(_)) => {
                match backend.snapshot(&receipt, previous.as_ref()) {
                    Ok(snapshot) => SampleResult {
                        logical_id,
                        ownership,
                        snapshot: Some(snapshot),
                        error: None,
                        completion: SampleCompletion::Completed,
                    },
                    Err(error) => SampleResult {
                        logical_id,
                        ownership,
                        snapshot: None,
                        error: Some(format!("{error:#}")),
                        completion: SampleCompletion::Completed,
                    },
                }
            }
            Ok(ownership) => SampleResult {
                logical_id,
                ownership,
                snapshot: None,
                error: None,
                completion: SampleCompletion::Completed,
            },
            Err(error) => reconciliation_failure(logical_id, &error),
        },
        Err(error) => SampleResult {
            logical_id,
            ownership: OwnershipState::Unsupported(format!("{error:#}")),
            snapshot: None,
            error: Some(format!("{error:#}")),
            completion: SampleCompletion::Completed,
        },
    }
}

fn run_bounded_sample(request: SampleRequest, sampler: Sampler, timeout: Duration) -> SampleResult {
    let logical_id = request.receipt.logical_id.clone();
    let cancelled = Arc::clone(&request.cancelled);
    if cancelled.load(Ordering::Acquire) {
        return interrupted_result(
            logical_id,
            SampleCompletion::Cancelled,
            "ownership verification cancelled; press r to retry".into(),
        );
    }
    let (result_tx, result_rx) = mpsc::sync_channel(1);
    let spawn = thread::Builder::new()
        .name("pa-resource-sample".into())
        .spawn(move || {
            let result = sampler(request.receipt, request.previous);
            let _ = result_tx.send(result);
        });
    if let Err(error) = spawn {
        return reconciliation_failure(
            logical_id,
            &anyhow::anyhow!("starting resource verification: {error}"),
        );
    }

    let deadline = Instant::now() + timeout;
    loop {
        if cancelled.load(Ordering::Acquire) {
            return interrupted_result(
                logical_id,
                SampleCompletion::Cancelled,
                "ownership verification cancelled; press r to retry".into(),
            );
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return interrupted_result(
                logical_id,
                SampleCompletion::TimedOut,
                format!("ownership verification timed out after {timeout:?}; press r to retry"),
            );
        }
        match result_rx.recv_timeout(remaining.min(CANCEL_POLL_INTERVAL)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                return reconciliation_failure(
                    logical_id,
                    &anyhow::anyhow!("resource verification worker exited without a result"),
                )
            }
        }
    }
}

impl ResourceWorker {
    pub fn start() -> Self {
        Self::start_with(SAMPLE_TIMEOUT, Arc::new(sample_receipt))
    }

    fn start_with(timeout: Duration, sampler: Sampler) -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<SampleRequest>(REQUEST_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel::<SampleResult>(RESULT_CAPACITY);
        let thread = thread::Builder::new()
            .name("pa-resource-coordinator".into())
            .spawn(move || {
                while let Ok(request) = request_rx.recv() {
                    let result = run_bounded_sample(request, Arc::clone(&sampler), timeout);
                    if result_tx.send(result).is_err() {
                        break;
                    }
                }
            })
            .expect("starting resource coordinator");
        Self {
            requests: Some(request_tx),
            results: Some(result_rx),
            cancellations: Mutex::new(Vec::new()),
            thread: Some(thread),
        }
    }

    #[cfg(test)]
    pub(crate) fn start_with_sampler<F>(timeout: Duration, sampler: F) -> Self
    where
        F: Fn(BindingReceipt, Option<ResourceSnapshot>) -> SampleResult + Send + Sync + 'static,
    {
        Self::start_with(timeout, Arc::new(sampler))
    }

    pub fn request(
        &self,
        receipt: BindingReceipt,
        previous: Option<ResourceSnapshot>,
    ) -> Option<SampleCancellation> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let request = SampleRequest {
            receipt,
            previous,
            cancelled: Arc::clone(&cancelled),
        };
        let Some(sender) = &self.requests else {
            return None;
        };
        match sender.try_send(request) {
            Ok(()) => {
                let mut cancellations = self
                    .cancellations
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                cancellations.retain(|entry| entry.strong_count() > 0);
                cancellations.push(Arc::downgrade(&cancelled));
                Some(SampleCancellation { cancelled })
            }
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => None,
        }
    }

    pub fn drain(&self) -> impl Iterator<Item = SampleResult> + '_ {
        self.results
            .as_ref()
            .into_iter()
            .flat_map(Receiver::try_iter)
    }
}

impl Drop for ResourceWorker {
    fn drop(&mut self) {
        let cancellations = self
            .cancellations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        for cancellation in cancellations.iter().filter_map(Weak::upgrade) {
            cancellation.store(true, Ordering::Release);
        }
        drop(cancellations);
        self.requests.take();
        self.results.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::supervision::{BackendKind, MuxTarget, ResourceLimits};
    use std::path::PathBuf;

    fn receipt() -> BindingReceipt {
        BindingReceipt {
            schema_version: 2,
            logical_id: LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "shell")
                .unwrap(),
            backend: BackendKind::SystemdUserService,
            unit_name: "portagenty-wtest-gtest.service".into(),
            invocation_id: "00".repeat(16),
            control_group: "/user.slice/test.service".into(),
            mux_target: MuxTarget::TmuxPrivate {
                socket: PathBuf::from("/tmp/test.sock"),
                session: "main".into(),
            },
            observed_at_unix_ms: 0,
            limits: ResourceLimits::default(),
            session_kind: None,
            requested_slice: None,
            workload_anchor: None,
            launch_boot_id: None,
        }
    }

    fn wait_for_result(worker: &ResourceWorker) -> SampleResult {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            if let Some(result) = worker.drain().next() {
                return result;
            }
            assert!(Instant::now() < deadline, "resource worker did not return");
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn reconciliation_errors_are_ambiguous_never_stale() {
        let logical_id =
            LogicalSessionId::new("550e8400-e29b-41d4-a716-446655440000", "shell").unwrap();
        let result = reconciliation_failure(logical_id, &anyhow::anyhow!("probe failed"));
        assert!(matches!(
            result.ownership,
            OwnershipState::AmbiguousBinding(ref reason) if reason.contains("probe failed")
        ));
        assert_eq!(result.error.as_deref(), Some("probe failed"));
        assert_eq!(result.completion, SampleCompletion::Completed);
    }

    #[test]
    fn bounded_worker_returns_completed_sample() {
        let worker =
            ResourceWorker::start_with_sampler(Duration::from_secs(1), |receipt, _| SampleResult {
                logical_id: receipt.logical_id,
                ownership: OwnershipState::IdleSupported,
                snapshot: None,
                error: None,
                completion: SampleCompletion::Completed,
            });
        let _cancellation = worker.request(receipt(), None).unwrap();
        let result = wait_for_result(&worker);
        assert_eq!(result.completion, SampleCompletion::Completed);
        assert!(matches!(result.ownership, OwnershipState::IdleSupported));
    }

    #[test]
    fn bounded_worker_times_out_blocked_sample() {
        let worker = ResourceWorker::start_with_sampler(Duration::from_millis(20), |receipt, _| {
            thread::sleep(Duration::from_millis(250));
            SampleResult {
                logical_id: receipt.logical_id,
                ownership: OwnershipState::IdleSupported,
                snapshot: None,
                error: None,
                completion: SampleCompletion::Completed,
            }
        });
        let _cancellation = worker.request(receipt(), None).unwrap();
        let result = wait_for_result(&worker);
        assert_eq!(result.completion, SampleCompletion::TimedOut);
        assert!(matches!(
            result.ownership,
            OwnershipState::AmbiguousBinding(ref reason) if reason.contains("timed out")
        ));
    }

    #[test]
    fn bounded_worker_cancels_blocked_sample() {
        let worker = ResourceWorker::start_with_sampler(Duration::from_secs(1), |receipt, _| {
            thread::sleep(Duration::from_millis(250));
            SampleResult {
                logical_id: receipt.logical_id,
                ownership: OwnershipState::IdleSupported,
                snapshot: None,
                error: None,
                completion: SampleCompletion::Completed,
            }
        });
        let cancellation = worker.request(receipt(), None).unwrap();
        cancellation.cancel();
        let result = wait_for_result(&worker);
        assert_eq!(result.completion, SampleCompletion::Cancelled);
        assert!(matches!(
            result.ownership,
            OwnershipState::AmbiguousBinding(ref reason) if reason.contains("cancelled")
        ));
    }
}
