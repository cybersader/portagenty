use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::{self, JoinHandle};

use crate::supervision::{
    BindingReceipt, LogicalSessionId, OwnershipState, ResourceSnapshot, SupervisionBackend,
};

const REQUEST_CAPACITY: usize = 8;
const RESULT_CAPACITY: usize = 8;

struct SampleRequest {
    receipt: BindingReceipt,
    previous: Option<ResourceSnapshot>,
}

#[derive(Debug)]
pub struct SampleResult {
    pub logical_id: LogicalSessionId,
    pub ownership: OwnershipState,
    pub snapshot: Option<ResourceSnapshot>,
    pub error: Option<String>,
}

/// One bounded worker for the lifetime of a session-list TUI. D-Bus and
/// cgroup filesystem reads stay off the render thread; duplicate requests are
/// dropped by the caller while one sample for the same logical session is in
/// flight. Results use bounded backpressure rather than being discarded, and
/// dropping the worker disconnects both channels before joining the thread.
pub struct ResourceWorker {
    requests: Option<SyncSender<SampleRequest>>,
    results: Option<Receiver<SampleResult>>,
    thread: Option<JoinHandle<()>>,
}

impl ResourceWorker {
    pub fn start() -> Self {
        let (request_tx, request_rx) = mpsc::sync_channel::<SampleRequest>(REQUEST_CAPACITY);
        let (result_tx, result_rx) = mpsc::sync_channel::<SampleResult>(RESULT_CAPACITY);
        let thread = thread::spawn(move || {
            let backend = crate::supervision::LinuxSystemdBackend::connect();
            while let Ok(request) = request_rx.recv() {
                let logical_id = request.receipt.logical_id.clone();
                let result = match &backend {
                    Ok(backend) => match backend.reconcile(&request.receipt) {
                        Ok(ownership @ OwnershipState::OwnedVerified(_)) => {
                            match backend.snapshot(&request.receipt, request.previous.as_ref()) {
                                Ok(snapshot) => SampleResult {
                                    logical_id,
                                    ownership,
                                    snapshot: Some(snapshot),
                                    error: None,
                                },
                                Err(error) => SampleResult {
                                    logical_id,
                                    ownership,
                                    snapshot: None,
                                    error: Some(format!("{error:#}")),
                                },
                            }
                        }
                        Ok(ownership) => SampleResult {
                            logical_id,
                            ownership,
                            snapshot: None,
                            error: None,
                        },
                        Err(error) => SampleResult {
                            logical_id,
                            ownership: OwnershipState::StaleBinding(format!("{error:#}")),
                            snapshot: None,
                            error: Some(format!("{error:#}")),
                        },
                    },
                    Err(error) => SampleResult {
                        logical_id,
                        ownership: OwnershipState::Unsupported(format!("{error:#}")),
                        snapshot: None,
                        error: Some(format!("{error:#}")),
                    },
                };
                if result_tx.send(result).is_err() {
                    break;
                }
            }
        });
        Self {
            requests: Some(request_tx),
            results: Some(result_rx),
            thread: Some(thread),
        }
    }

    pub fn request(&self, receipt: BindingReceipt, previous: Option<ResourceSnapshot>) -> bool {
        let Some(sender) = &self.requests else {
            return false;
        };
        match sender.try_send(SampleRequest { receipt, previous }) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => false,
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
        self.requests.take();
        self.results.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
