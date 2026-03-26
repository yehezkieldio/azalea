//! Metrics aggregation and persistence.
//!
//! ## Concurrency assumptions
//! All counters are atomic and can be updated from multiple tasks.
//!
//! ## Trade-off acknowledgment
//! Metrics are best-effort; failures are logged but do not halt the pipeline.

use super::{FlushWorker, open_or_rotate_corrupt_store};
use redb::{AccessGuard, Database, ReadableDatabase, ReadableTable, TableDefinition};
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

const METRICS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("metrics");
const ERRORS_TABLE: TableDefinition<&str, u64> = TableDefinition::new("errors");
const ERROR_CATEGORY_COUNT: usize = 6;

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

/// Error categories tracked in metrics storage.
///
/// The set is intentionally closed so the update path stays allocation-free
/// and bounded under steady-state load.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCategory {
    ResolveFailed,
    DownloadFailed,
    SsrfBlocked,
    OptimizeFailed,
    UploadFailed,
    ProgressUpdateFailed,
}

/// Snapshot of counters used for status and statistics reporting.
///
/// `totals` are cumulative for the lifetime of the persisted tracker.
/// `stage_window` is the current in-memory timing window and resets at startup
/// and after each successful flush.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot {
    pub totals: TotalsSnapshot,
    pub stage_window: StageWindowSnapshot,
}

/// Lifetime counters persisted cumulatively.
#[derive(Debug, Clone, Copy)]
pub struct TotalsSnapshot {
    pub total_runs: u64,
    pub successes: u64,
    pub failures: u64,
}

/// Current stage timing window used for rolling averages.
///
/// ## Invariants
/// `avg_ms` entries are zero when the corresponding `sample_count` is zero.
#[derive(Debug, Clone, Copy)]
pub struct StageWindowSnapshot {
    pub avg_ms: [u64; 4],
    pub sample_count: [u64; 4],
}

struct LoadState {
    total_runs: u64,
    successes: u64,
    failures: u64,
    errors: [u64; ERROR_CATEGORY_COUNT],
}

impl Stage {
    pub const ALL: [Self; 4] = [Self::Resolve, Self::Download, Self::Optimize, Self::Upload];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Resolve => "resolve",
            Self::Download => "download",
            Self::Optimize => "optimize",
            Self::Upload => "upload",
        }
    }
}

impl ErrorCategory {
    pub const ALL: [Self; ERROR_CATEGORY_COUNT] = [
        Self::ResolveFailed,
        Self::DownloadFailed,
        Self::SsrfBlocked,
        Self::OptimizeFailed,
        Self::UploadFailed,
        Self::ProgressUpdateFailed,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ResolveFailed => "resolve_failed",
            Self::DownloadFailed => "download_failed",
            Self::SsrfBlocked => "ssrf_blocked",
            Self::OptimizeFailed => "optimize_failed",
            Self::UploadFailed => "upload_failed",
            Self::ProgressUpdateFailed => "progress_update_errors",
        }
    }

    pub fn from_persisted_str(value: &str) -> Option<Self> {
        match value {
            "resolve_failed" => Some(Self::ResolveFailed),
            "download_failed" => Some(Self::DownloadFailed),
            "ssrf_blocked" => Some(Self::SsrfBlocked),
            "optimize_failed" => Some(Self::OptimizeFailed),
            "upload_failed" => Some(Self::UploadFailed),
            "progress_update_errors" => Some(Self::ProgressUpdateFailed),
            _ => None,
        }
    }

    fn increment(self, counts: &[AtomicU64; ERROR_CATEGORY_COUNT]) {
        self.atomic_slot(counts).fetch_add(1, Ordering::Relaxed);
    }

    fn load_atomic(self, counts: &[AtomicU64; ERROR_CATEGORY_COUNT]) -> u64 {
        self.atomic_slot(counts).load(Ordering::Relaxed)
    }

    fn store_atomic(self, counts: &[AtomicU64; ERROR_CATEGORY_COUNT], value: u64) {
        self.atomic_slot(counts).store(value, Ordering::Relaxed);
    }

    fn load_value(self, counts: &[u64; ERROR_CATEGORY_COUNT]) -> u64 {
        *self.value_slot(counts)
    }

    fn store_value(self, counts: &mut [u64; ERROR_CATEGORY_COUNT], value: u64) {
        *self.value_slot_mut(counts) = value;
    }

    fn atomic_slot(self, counts: &[AtomicU64; ERROR_CATEGORY_COUNT]) -> &AtomicU64 {
        match self {
            Self::ResolveFailed => &counts[0],
            Self::DownloadFailed => &counts[1],
            Self::SsrfBlocked => &counts[2],
            Self::OptimizeFailed => &counts[3],
            Self::UploadFailed => &counts[4],
            Self::ProgressUpdateFailed => &counts[5],
        }
    }

    fn value_slot(self, counts: &[u64; ERROR_CATEGORY_COUNT]) -> &u64 {
        match self {
            Self::ResolveFailed => &counts[0],
            Self::DownloadFailed => &counts[1],
            Self::SsrfBlocked => &counts[2],
            Self::OptimizeFailed => &counts[3],
            Self::UploadFailed => &counts[4],
            Self::ProgressUpdateFailed => &counts[5],
        }
    }

    fn value_slot_mut(self, counts: &mut [u64; ERROR_CATEGORY_COUNT]) -> &mut u64 {
        match self {
            Self::ResolveFailed => &mut counts[0],
            Self::DownloadFailed => &mut counts[1],
            Self::SsrfBlocked => &mut counts[2],
            Self::OptimizeFailed => &mut counts[3],
            Self::UploadFailed => &mut counts[4],
            Self::ProgressUpdateFailed => &mut counts[5],
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
    error_counts: [AtomicU64; ERROR_CATEGORY_COUNT],
    flush_worker: FlushWorker,
}

impl Tracker {
    /// Initialize metrics storage; returns a disabled instance if configured off.
    ///
    /// ## Explicit non-goals
    /// This does not create background flush tasks; call [`Tracker::start_flush_task`].
    pub fn new(config: &crate::config::StorageSettings) -> anyhow::Result<Self> {
        if !config.metrics_enabled {
            return Ok(Self {
                inner: Arc::new(Inner::new(false, None)),
            });
        }

        let db = open_or_rotate_corrupt_store("metrics", &config.metrics_db_path, |db| {
            let write_txn = db.begin_write().map_err(redb::Error::from)?;
            {
                let _ = write_txn
                    .open_table(METRICS_TABLE)
                    .map_err(redb::Error::from)?;
                let _ = write_txn
                    .open_table(ERRORS_TABLE)
                    .map_err(redb::Error::from)?;
            }
            write_txn.commit().map_err(redb::Error::from)?;
            Ok(())
        })?;

        Ok(Self {
            inner: Arc::new(Inner::new(true, Some(Arc::new(db)))),
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

    /// Record an error category for aggregation.
    pub fn record_error(&self, category: ErrorCategory) {
        if !self.inner.enabled {
            return;
        }
        category.increment(&self.inner.error_counts);
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
            let mut errors = [0; ERROR_CATEGORY_COUNT];

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
            }

            if let Ok(table) = read_txn.open_table(ERRORS_TABLE) {
                for entry in table.iter()? {
                    let (key, value): (AccessGuard<&str>, AccessGuard<u64>) = entry?;
                    if let Some(category) = ErrorCategory::from_persisted_str(key.value()) {
                        category.store_value(&mut errors, value.value());
                    }
                }
            }

            Ok(LoadState {
                total_runs,
                successes,
                failures,
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

        for slot in self.inner.stage_duration_sum_ms.iter() {
            slot.store(0, Ordering::Relaxed);
        }
        for slot in self.inner.stage_count.iter() {
            slot.store(0, Ordering::Relaxed);
        }
        for category in ErrorCategory::ALL {
            category.store_atomic(
                &self.inner.error_counts,
                category.load_value(&result.errors),
            );
        }

        Ok(())
    }

    /// Read a snapshot without blocking writers.
    ///
    /// ## Time complexity
    /// $O(1)$ over a fixed number of stages.
    pub fn snapshot(&self) -> Snapshot {
        Snapshot {
            totals: TotalsSnapshot {
                total_runs: self.inner.total_runs.load(Ordering::Relaxed),
                successes: self.inner.successes.load(Ordering::Relaxed),
                failures: self.inner.failures.load(Ordering::Relaxed),
            },
            stage_window: StageWindowSnapshot {
                avg_ms: std::array::from_fn(|idx| {
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
                }),
                sample_count: std::array::from_fn(|idx| {
                    self.inner
                        .stage_count
                        .get(idx)
                        .map(|slot| slot.load(Ordering::Relaxed))
                        .unwrap_or(0)
                }),
            },
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
        let error_counts = ErrorCategory::ALL
            .map(|category| (category, category.load_atomic(&self.inner.error_counts)));

        let result = tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            // Snapshot values are written as a single transaction for consistency.
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(METRICS_TABLE)?;
                let _ = table.insert("total_runs", total_runs);
                let _ = table.insert("successes", successes);
                let _ = table.insert("failures", failures);
            }

            {
                let mut table = write_txn.open_table(ERRORS_TABLE)?;
                table.retain(|key, _| ErrorCategory::from_persisted_str(key).is_some())?;
                for (category, count) in error_counts {
                    let _ = table.insert(category.as_str(), count);
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
        if !self.inner.enabled {
            return;
        }

        let metrics = self.clone();
        self.inner
            .flush_worker
            .start(interval, move || {
                let metrics = metrics.clone();
                async move {
                    metrics.flush().await;
                }
            })
            .await;
    }

    /// Stop the periodic flush task.
    pub async fn stop_flush_task(&self) {
        self.inner.flush_worker.stop().await;
    }
}

impl Inner {
    fn new(enabled: bool, db: Option<Arc<Database>>) -> Self {
        Self {
            enabled,
            db,
            total_runs: AtomicU64::new(0),
            successes: AtomicU64::new(0),
            failures: AtomicU64::new(0),
            stage_duration_sum_ms: std::array::from_fn(|_| AtomicU64::new(0)),
            stage_count: std::array::from_fn(|_| AtomicU64::new(0)),
            error_counts: std::array::from_fn(|_| AtomicU64::new(0)),
            flush_worker: FlushWorker::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]
    use super::*;

    fn unique_metrics_path(name: &str) -> std::path::PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        std::env::temp_dir().join(format!("azalea-{name}-{nanos}.redb"))
    }

    fn storage_config(
        metrics_enabled: bool,
        metrics_db_path: std::path::PathBuf,
    ) -> crate::config::StorageSettings {
        crate::config::StorageSettings {
            metrics_enabled,
            metrics_db_path,
            ..crate::config::StorageSettings::default()
        }
    }

    fn rotated_store_paths(path: &std::path::Path) -> Vec<std::path::PathBuf> {
        let Some(file_name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            return Vec::new();
        };
        let prefix = format!("{file_name}.corrupt.");
        let Some(parent) = path.parent() else {
            return Vec::new();
        };

        let mut rotated = std::fs::read_dir(parent)
            .expect("parent directory should be readable")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|candidate| {
                candidate
                    .file_name()
                    .and_then(std::ffi::OsStr::to_str)
                    .is_some_and(|name| name.starts_with(&prefix))
            })
            .collect::<Vec<_>>();
        rotated.sort();
        rotated
    }

    #[test]
    fn disabled_metrics_are_noop() {
        let path = unique_metrics_path("metrics-disabled");
        let tracker = Tracker::new(&storage_config(false, path)).expect("tracker should construct");
        tracker.record_success();
        tracker.record_failure();
        tracker.record_stage_duration(Stage::Resolve, 123);
        tracker.record_error(ErrorCategory::ResolveFailed);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.totals.total_runs, 0);
        assert_eq!(snapshot.totals.successes, 0);
        assert_eq!(snapshot.totals.failures, 0);
        assert_eq!(snapshot.stage_window.avg_ms, [0; 4]);
        assert_eq!(snapshot.stage_window.sample_count, [0; 4]);
    }

    #[test]
    fn snapshot_aggregates_counters_and_stage_averages() {
        let path = unique_metrics_path("metrics-snapshot");
        let tracker =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should construct");
        tracker.record_success();
        tracker.record_failure();
        tracker.record_stage_duration(Stage::Resolve, 100);
        tracker.record_stage_duration(Stage::Resolve, 300);
        tracker.record_stage_duration(Stage::Upload, 80);

        let snapshot = tracker.snapshot();
        assert_eq!(snapshot.totals.total_runs, 2);
        assert_eq!(snapshot.totals.successes, 1);
        assert_eq!(snapshot.totals.failures, 1);
        assert_eq!(
            snapshot
                .stage_window
                .avg_ms
                .get(Stage::Resolve as usize)
                .copied(),
            Some(200)
        );
        assert_eq!(
            snapshot
                .stage_window
                .avg_ms
                .get(Stage::Upload as usize)
                .copied(),
            Some(80)
        );
        assert_eq!(
            snapshot
                .stage_window
                .sample_count
                .get(Stage::Resolve as usize)
                .copied(),
            Some(2)
        );
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn flush_then_load_restores_persisted_state() {
        let path = unique_metrics_path("metrics-persist");
        let tracker =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should construct");
        tracker.record_success();
        tracker.record_stage_duration(Stage::Download, 250);
        tracker.record_error(ErrorCategory::DownloadFailed);
        tracker.flush().await;
        drop(tracker);

        let restored =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should reconstruct");
        restored.load_from_db().await.expect("load should succeed");

        let snapshot = restored.snapshot();
        assert_eq!(snapshot.totals.total_runs, 1);
        assert_eq!(snapshot.totals.successes, 1);
        assert_eq!(snapshot.totals.failures, 0);
        assert_eq!(snapshot.stage_window.avg_ms, [0; 4]);
        assert_eq!(snapshot.stage_window.sample_count, [0; 4]);
        assert_eq!(
            ErrorCategory::DownloadFailed.load_atomic(&restored.inner.error_counts),
            1
        );

        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn load_ignores_unknown_persisted_error_keys() {
        let path = unique_metrics_path("metrics-legacy-errors");
        let tracker =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should construct");
        tracker.record_error(ErrorCategory::ResolveFailed);
        tracker.flush().await;

        let db = tracker
            .inner
            .db
            .as_ref()
            .cloned()
            .expect("enabled tracker should have a db");
        tokio::task::spawn_blocking(move || -> Result<(), redb::Error> {
            let write_txn = db.begin_write()?;
            {
                let mut table = write_txn.open_table(ERRORS_TABLE)?;
                let _ = table.insert("legacy_error_key", 99);
            }
            write_txn.commit()?;
            Ok(())
        })
        .await
        .expect("blocking task should join")
        .expect("manual write should succeed");
        drop(tracker);

        let restored =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should reconstruct");
        restored.load_from_db().await.expect("load should succeed");

        assert_eq!(
            ErrorCategory::ResolveFailed.load_atomic(&restored.inner.error_counts),
            1
        );
        assert_eq!(
            ErrorCategory::DownloadFailed.load_atomic(&restored.inner.error_counts),
            0
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn corrupt_metrics_store_is_rotated_and_recreated() {
        let path = unique_metrics_path("metrics-corrupt-rotate");
        let original = b"metrics-corruption-evidence";
        std::fs::write(&path, original).expect("corrupt seed should be written");

        let tracker =
            Tracker::new(&storage_config(true, path.clone())).expect("tracker should recreate db");

        assert!(
            tracker.inner.enabled,
            "metrics tracker should remain enabled"
        );
        assert!(path.exists(), "fresh metrics store should exist");

        let rotated = rotated_store_paths(&path);
        assert_eq!(rotated.len(), 1, "corrupt store should be preserved once");
        let rotated_path = rotated
            .first()
            .cloned()
            .expect("corrupt store should have been rotated");
        assert_eq!(
            std::fs::read(&rotated_path).expect("rotated store should be readable"),
            original
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(rotated_path);
    }
}
