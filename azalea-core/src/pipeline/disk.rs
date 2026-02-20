//! Disk space guardrails for pipeline stages.
//!
//! ## Design rationale
//! The pipeline pre-checks available disk to avoid partially downloaded files
//! and to keep transcode stages from thrashing temp storage.
//!
//! ## Concurrency assumptions
//! The check is performed in a blocking task to avoid stalling async reactors.
//!
//! ## References
//! - `statvfs`: <https://pubs.opengroup.org/onlinepubs/9699919799/functions/statvfs.html>

use std::path::{Path, PathBuf};

use crate::pipeline::Error;

/// Validate that the filesystem backing `path` has sufficient free space.
///
/// ## Preconditions
/// - `path` points to an existing directory or file within the target FS.
///
/// ## Postconditions
/// - Returns `Ok(())` when `required_bytes` can be satisfied.
/// - Returns `Error::DiskSpace` with values expressed in MiB.
pub async fn ensure_disk_space(path: &Path, required_bytes: u64) -> Result<(), Error> {
    if required_bytes == 0 {
        return Ok(());
    }

    let path = PathBuf::from(path);
    let available: Option<u64> = tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use nix::sys::statvfs::statvfs;

            statvfs(&path)
                .ok()
                .map(|stat| stat.blocks_available().saturating_mul(stat.fragment_size()))
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
    .await
    .unwrap_or(None);

    if let Some(available_bytes) = available
        && available_bytes < required_bytes
    {
        return Err(Error::DiskSpace {
            available_mb: available_bytes / 1024 / 1024,
            required_mb: required_bytes / 1024 / 1024,
        });
    }

    Ok(())
}
