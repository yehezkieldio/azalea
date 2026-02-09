//! Metrics aggregation and persistence.
//!
//! ## Concurrency assumptions
//! All counters are atomic and can be updated from multiple tasks.
//!
//! ## Trade-off acknowledgment
//! Metrics are best-effort; failures are logged but do not halt the pipeline.

use dashmap::DashMap;
use redb::{AccessGuard, Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::{Mutex, watch};

const METRICS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metrics");
const ERRORS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("errors");
const MAX_ERROR_KEYS: usize = 128;

/// Pipeline stages tracked in metrics storage.
///
/// ## Domain terminology
/// Stage names map to pipeline steps (`resolve`, `download`, `optimize`, `upload`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Resolve = 0,
    Download = 1,
    Optimize = 2,
    Upload = 3,
}

/// Snapshot of counters used for status and statistics reporting.
///
/// ## Invariants
/// `stage_avg_ms` entries are zero when no samples exist.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub total_runs: u64,
    pub successes: u64,
    pub failures: u64,
    pub stage_avg_ms: [u64; 4],
}

struct LoadState {
    total_runs: u64,
    successes: u64,
    failures: u64,
    stage_sum: [u64; 4],
    stage_count: [u64; 4],
    errors: Vec<(String, u64)>,
}

impl Stage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Download => "download",
            Self::Optimize => "optimize",
            Self::Upload => "upload",
        }
    }
}

/// In-memory metrics with optional persistence to redb.
///
/// ## Usage footguns
/// Call [`Tracker::load_from_db`] at startup to hydrate counters for status views.
#[derive(Clone)]
pub struct Tracker {
    inner: Arc<Inner>,
}

struct Inner {
    enabled: bool,
    db: Option<Arc<Database>>,
    total_runs: AtomicU64,
    successes: AtomicU64,
    failures: AtomicU64,
    stage_duration_sum_ms: [AtomicU64; 4],
    stage_count: [AtomicU64; 4],
    error_counts: DashMap<String, AtomicU64>,
    flush_cancel: watch::Sender<bool>,
    flush_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Tracker {
    /// Initialize metrics storage; returns a disabled instance if configured off.
    ///
    /// ## Explicit non-goals
    /// This does not create background flush tasks; call [`Tracker::start_flush_task`].
    pub fn new(config: &crate::config::StorageSettings) -> anyhow::Result<Self> {
        if !config.metrics_enabled {
            return Ok(Self {
                inner: Arc::new(Inner::disabled()),
            });
        }

        // Opens existing DBs or initializes new ones.
        let db = Database::create(&config.metrics_db_path)?;
        let write_txn = db.begin_write()?;
        {
            let _ = write_txn.open_table(METRICS_TABLE)?;
            let _ = write_txn.open_table(ERRORS_TABLE)?;
        }
        write_txn.commit()?;

        Ok(Self {
            inner: Arc::new(Inner::new(Some(Arc::new(db)))),
        })
    }

    /// Record a successful pipeline run.
    pub fn record_success(&self) {
        if !self.inner.enabled {
            return;
        }
        self.inner.total_runs.fetch_add(1, Ordering::Relaxed);
        self.inner.successes.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a failed pipeline run.
    pub fn record_failure(&self) {
        if !self.inner.enabled {
            return;
        }
        self.inner.total_runs.fetch_add(1, Ordering::Relaxed);
        self.inner.failures.fetch_add(1, Ordering::Relaxed);
    }

    /// Record a single stage duration in milliseconds.
    ///
    /// ## Hot-path markers
    /// Called for every pipeline stage; keep overhead minimal.
    pub fn record_stage_duration(&self, stage: Stage, duration_ms: u64) {
        if !self.inner.enabled {
            return;
        }
        let idx = stage as usize;
        // Fixed index mapping keeps this hot path allocation-free.
        if let (Some(sum), Some(count)) = (
            self.inner.stage_duration_sum_ms.get(idx),
            self.inner.stage_count.get(idx),
        ) {
            sum.fetch_add(duration_ms, Ordering::Relaxed);
            count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Record an error kind for aggregation.
    pub fn record_error(&self, kind: &'static str) {
        if !self.inner.enabled {
            return;
        }

        if self.inner.error_counts.len() >= MAX_ERROR_KEYS
            && self.inner.error_counts.get(kind).is_none()
        {
            tracing::warn!(kind, "Skipping metrics error key; cap reached");
            return;
        }

        self.inner
            .error_counts
            .entry(kind.to_string())
            .or_insert_with(|| AtomicU64::new(0))
            .value()
            .fetch_add(1, Ordering::Relaxed);
    }

    /// Load counters from the backing store into memory.
    ///
    /// ## Postconditions
    /// In-memory counters reflect persisted state for status reporting.
    pub async fn load_from_db(&self) -> anyhow::Result<()> {
        if !self.inner.enabled {
            return Ok(());
        }

        let Some(db) = &self.inner.db else {
            return Ok(());
        };

        let db = Arc::clone(db);
        let result = tokio::task::spawn_blocking(move || -> anyhow::Result<LoadState> {
            let read_txn = db.begin_read()?;

            let mut total_runs = 0;
            let mut successes = 0;
            let mut failures = 0;
            let mut stage_sum = [0u64; 4];
            let mut stage_count = [0u64; 4];
            let mut errors = Vec::new();

            if let Ok(table) = read_txn.open_table(METRICS_TABLE) {
                if let Ok(Some(value)) = table.get("total_runs") {
                    total_runs = value.value();
                }
                if let Ok(Some(value)) = table.get("successes") {
                    successes = value.value();
                }
                if let Ok(Some(value)) = table.get("failures") {
                    failures = value.value();
                }

                for stage in [
                    Stage::Resolve,
                    Stage::Download,
                    Stage::Optimize,
                    Stage::Upload,
                ] {
                    let idx = stage as usize;
                    let sum_key = format!("stage.{}.duration_sum_ms", stage.as_str());
                    let count_key = format!("stage.{}.count", stage.as_str());

                    if let Ok(Some(value)) = table.get(sum_key.as_str())
                        && let Some(slot) = stage_sum.get_mut(idx)
                    {
                        *slot = value.value();
                    }
                    if let Ok(Some(value)) = table.get(count_key.as_str())
                        && let Some(slot) = stage_count.get_mut(idx)
                    {
                        *slot = value.value();
                    }
                }
            }

            if let Ok(table) = read_txn.open_table(ERRORS_TABLE) {
                for entry in table.iter()? {
                    let (key, value): (AccessGuard<&str>, AccessGuard<u64>) = entry?;
                    errors.push((key.value().to_string(), value.value()));
                }
            }

            Ok(LoadState {
                total_runs,
                successes,
                failures,
                stage_sum,
                stage_count,
                errors,
            })
        })
        .await
        .map_err(|e| anyhow::anyhow!(e))??;

        self.inner
            .total_runs
            .store(result.total_runs, Ordering::Relaxed);
        self.inner
            .successes
            .store(result.successes, Ordering::Relaxed);
        self.inner
            .failures
            .store(result.failures, Ordering::Relaxed);

        for ((sum_slot, count_slot), (sum, count)) in self
            .inner
            .stage_duration_sum_ms
            .iter()
            .zip(self.inner.stage_count.iter())
            .zip(result.stage_sum.iter().zip(result.stage_count.iter()))
        {
            sum_slot.store(*sum, Ordering::Relaxed);
            count_slot.store(*count, Ordering::Relaxed);
        }

        self.inner.error_counts.clear();
        for (key, value) in result.errors {
            self.inner.error_counts.insert(key, AtomicU64::new(value));
        }

        Ok(())
    }

    /// Read a snapshot without blocking writers.
    ///
    /// ## Time complexity
    /// $O(1)$ over a fixed number of stages.
    pub fn snapshot(&self) -> Snapshot {
        let total_runs = self.inner.total_runs.load(Ordering::Relaxed);
        let successes = self.inner.successes.load(Ordering::Relaxed);
        let failures = self.inner.failures.load(Ordering::Relaxed);

        let stage_avg_ms = std::array::from_fn(|idx| {
            let sum = self
                .inner
                .stage_duration_sum_ms
                .get(idx)
                .map(|slot| slot.load(Ordering::Relaxed))
                .unwrap_or(0);
            let count = self
                .inner
                .stage_count
                .get(idx)
                .map(|slot| slot.load(Ordering::Relaxed))
                .unwrap_or(0);
            // Avoid division by zero when no samples exist.
            sum.checked_div(count).unwrap_or(0)
        });

        Snapshot {
            total_runs,
            successes,
            failures,
            stage_avg_ms,
        }
    }

    /// Flush in-memory counters to the backing store.
    pub async fn flush(&self) {
        if !self.inner.enabled {
            return;
        }

        let Some(db) = &self.inner.db else {
            return;
        };
        let db = Arc::clone(db);

        let total_runs = self.inner.total_runs.load(Ordering::Relaxed);
        let successes = self.inner.successes.load(Ordering::Relaxed);
        let failures = self.inner.failures.load(Ordering::Relaxed);

        let stage_data: Vec<(String, u64, u64)> = [
            Stage::Resolve,
            Stage::Download,
            Stage::Optimize,
            Stage::Upload,
        ]
        .iter()
        .map(|stage| {
            let idx = *stage as usize;
            (
                stage.as_str().to_string(),
                self.inner
                    .stage_duration_sum_ms
                    .get(idx)
                    .map(|slot| slot.load(Ordering::Relaxed))
                    .unwrap_or(0),
                self.inner
                    .stage_count
                    .get(idx)
                    .map(|slot| slot.load(Ordering::Relaxed))
                    .unwrap_or(0),
            )
        })
        .collect();

        let error_counts: Vec<(String, u64)> = self
            .inner
            .error_counts
            .iter()
            .map(|entry| {
                (
                    entry.key().to_string(),
                    entry.value().load(Ordering::Relaxed),
                )
            })
            .collect();

        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            // Snapshot values are written as a single transaction for consistency.
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(METRICS_TABLE)?;
                let _ = table.insert("total_runs", total_runs);
                let _ = table.insert("successes", successes);
                let _ = table.insert("failures", failures);

                for (stage, sum, count) in stage_data {
                    let sum_key = format!("stage.{}.duration_sum_ms", stage);
                    let count_key = format!("stage.{}.count", stage);
                    let _ = table.insert(sum_key.as_str(), sum);
                    let _ = table.insert(count_key.as_str(), count);
                }
            }

            {
                let mut table = write_txn.open_table(ERRORS_TABLE)?;
                for (key, count) in error_counts {
                    let _ = table.insert(key.as_str(), count);
                }
            }

            write_txn.commit()?;
            Ok(())
        })
        .await;

        match result {
            Ok(Ok(())) => {
                // Reset stage aggregates after flush to keep averages recent.
                for slot in self.inner.stage_duration_sum_ms.iter() {
                    slot.store(0, Ordering::Relaxed);
                }
                for slot in self.inner.stage_count.iter() {
                    slot.store(0, Ordering::Relaxed);
                }
            }
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "Failed to flush metrics");
            }
            Err(e) => {
                tracing::warn!(error = %e, "Failed to flush metrics");
            }
        }
    }

    /// Start a periodic flush task; no-op when disabled.
    pub async fn start_flush_task(&self, interval: std::time::Duration) {
        if !self.inner.enabled || interval.is_zero() {
            return;
        }

        let mut handle_guard = self.inner.flush_handle.lock().await;
        if handle_guard.is_some() {
            return;
        }

        let _ = self.inner.flush_cancel.send(false);
        let metrics = self.clone();
        let mut cancel_rx = self.inner.flush_cancel.subscribe();
        *handle_guard = Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            loop {
                tokio::select! {
                    _ = cancel_rx.changed() => {
                        if *cancel_rx.borrow() {
                            break;
                        }
                    }
                    _ = ticker.tick() => {
                        metrics.flush().await;
                    }
                }
            }
        }));
    }

    /// Stop the periodic flush task.
    pub async fn stop_flush_task(&self) {
        let _ = self.inner.flush_cancel.send(true);
        if let Some(handle) = self.inner.flush_handle.lock().await.take() {
            let _ = handle.await;
        }
    }
}

impl Inner {
    fn new(db: Option<Arc<Database>>) -> Self {
        let (flush_cancel, _) = watch::channel(false);
        Self {
            enabled: true,
            db,
            total_runs: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            stage_duration_sum_ms: std::array::from_fn(|_| AtomicU64::new(0)),
            stage_count: std::array::from_fn(|_| AtomicU64::new(0)),
            error_counts: DashMap::new(),
            flush_cancel,
            flush_handle: Mutex::new(None),
        }
    }

    fn disabled() -> Self {
        let (flush_cancel, _) = watch::channel(false);
        Self {
            enabled: false,
            db: None,
            total_runs: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            stage_duration_sum_ms: std::array::from_fn(|_| AtomicU64::new(0)),
            stage_count: std::array::from_fn(|_| AtomicU64::new(0)),
            error_counts: DashMap::new(),
            flush_cancel,
            flush_handle: Mutex::new(None),
        }
    }
}
