use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::{Mutex, Notify, watch};

pub mod dedup;
pub mod metrics;

pub use dedup::Cache as DedupCache;
pub use metrics::{Snapshot as MetricsSnapshot, Stage, Tracker as Metrics};

/// Shared flush worker for storage-backed subsystems.
///
/// This stays concrete and local to storage so dedup and metrics can share
/// queue wakeups, periodic flushing, and shutdown without extra indirection.
#[derive(Debug)]
pub(crate) struct FlushWorker {
    running: AtomicBool,
    wake: Arc<Notify>,
    cancel: watch::Sender<bool>,
    handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl FlushWorker {
    pub(crate) fn new() -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            running: AtomicBool::new(false),
            wake: Arc::new(Notify::new()),
            cancel,
            handle: Mutex::new(None),
        }
    }

    pub(crate) fn schedule(&self) -> bool {
        if !self.running.load(Ordering::Relaxed) {
            return false;
        }
        self.wake.notify_one();
        true
    }

    pub(crate) async fn start<F, Fut>(&self, interval: Duration, flush: F)
    where
        F: Fn() -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        if interval.is_zero() || self.running.swap(true, Ordering::Relaxed) {
            return;
        }

        let mut handle_guard = self.handle.lock().await;
        if handle_guard.is_some() {
            return;
        }

        let _ = self.cancel.send(false);
        let wake = Arc::clone(&self.wake);
        let mut cancel_rx = self.cancel.subscribe();
        *handle_guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            // Skip the immediate tick so startup does not flush empty or partial state.
            ticker.tick().await;

            loop {
                tokio::select! {
                    biased;
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    _ = wake.notified() => {
                        flush().await;
                    }
                    _ = ticker.tick() => {
                        flush().await;
                    }
                }
            }
        }));
    }

    pub(crate) async fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
        let _ = self.cancel.send(true);
        if let Some(handle) = self.handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}
