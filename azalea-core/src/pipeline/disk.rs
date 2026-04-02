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
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};

use crate::pipeline::Error;

#[derive(Debug)]
pub struct DownloadReservation {
    reserved_download_bytes: Arc<AtomicU64>,
    reserved_bytes: u64,
}

impl Drop for DownloadReservation {
    fn drop(&mut self) {
        if self.reserved_bytes == 0 {
            return;
        }

        self.reserved_download_bytes
            .fetch_sub(self.reserved_bytes, Ordering::AcqRel);
    }
}

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

    let path_display = path.display().to_string();

    tracing::trace!(
        path = %path_display,
        required_bytes,
        "Checking disk space"
    );

    let available = available_disk_bytes(path).await;

    if let Some(available_bytes) = available
        && available_bytes < required_bytes
    {
        tracing::warn!(
            path = %path_display,
            available_bytes,
            required_bytes,
            "Insufficient disk space for pipeline stage"
        );
        return Err(Error::DiskSpace {
            available_mb: available_bytes / 1024 / 1024,
            required_mb: required_bytes / 1024 / 1024,
        });
    }

    if let Some(available_bytes) = available {
        tracing::trace!(available_bytes, required_bytes, "Disk space check passed");
    } else {
        tracing::trace!("Disk space availability could not be determined on this platform");
    }

    Ok(())
}

/// Reserve download bytes against a shared in-flight counter before writing.
///
/// ## Invariants
/// - The returned guard releases its reservation on drop.
/// - Reservation checks preserve `min_free_bytes` after accounting for all
///   currently reserved downloads tracked in `reserved_download_bytes`.
pub async fn reserve_download_bytes(
    path: &Path,
    min_free_bytes: u64,
    reserve_bytes: u64,
    reserved_download_bytes: &Arc<AtomicU64>,
) -> Result<DownloadReservation, Error> {
    if reserve_bytes == 0 {
        return Ok(DownloadReservation {
            reserved_download_bytes: Arc::clone(reserved_download_bytes),
            reserved_bytes: 0,
        });
    }

    let path_display = path.display().to_string();
    let path = PathBuf::from(path);

    loop {
        let currently_reserved = reserved_download_bytes.load(Ordering::Acquire);
        let next_reserved =
            currently_reserved
                .checked_add(reserve_bytes)
                .ok_or(Error::DiskSpace {
                    available_mb: 0,
                    required_mb: u64::MAX / 1024 / 1024,
                })?;
        let required_bytes = min_free_bytes.saturating_add(next_reserved);
        let available = available_disk_bytes(path.as_path()).await;

        if let Some(available_bytes) = available
            && available_bytes < required_bytes
        {
            tracing::warn!(
                path = %path_display,
                available_bytes,
                required_bytes,
                reserve_bytes,
                currently_reserved,
                "Insufficient disk space for collectively bounded downloads"
            );
            return Err(Error::DiskSpace {
                available_mb: available_bytes / 1024 / 1024,
                required_mb: required_bytes / 1024 / 1024,
            });
        }

        match reserved_download_bytes.compare_exchange(
            currently_reserved,
            next_reserved,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {
                tracing::trace!(
                    path = %path_display,
                    reserve_bytes,
                    total_reserved_bytes = next_reserved,
                    "Reserved download disk budget"
                );
                return Ok(DownloadReservation {
                    reserved_download_bytes: Arc::clone(reserved_download_bytes),
                    reserved_bytes: reserve_bytes,
                });
            }
            Err(_) => {
                tracing::trace!(
                    path = %path_display,
                    reserve_bytes,
                    "Retrying download disk reservation after concurrent update"
                );
            }
        }
    }
}

async fn available_disk_bytes(path: &Path) -> Option<u64> {
    let path = PathBuf::from(path);
    tokio::task::spawn_blocking(move || {
        #[cfg(unix)]
        {
            use nix::sys::statvfs::statvfs;

            statvfs(&path)
                .ok()
                .map(|stat| stat.blocks_available().saturating_mul(stat.fragment_size()))
        }
        #[cfg(windows)]
        {
            available_disk_bytes_windows(&path)
        }
        #[cfg(not(any(unix, windows)))]
        {
            None
        }
    })
    .await
    .unwrap_or(None)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn available_disk_bytes_windows(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let query_path = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let mut free_bytes = 0_u64;
    let wide = query_path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();

    // Safety: `wide` is NUL-terminated and remains alive for the duration of
    // the call, and the output pointers refer to initialized local storage.
    let success = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut free_bytes,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    if success == 0 {
        return None;
    }

    Some(free_bytes)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::{available_disk_bytes, reserve_download_bytes};
    use crate::pipeline::Error;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    #[tokio::test]
    async fn collective_reservations_fail_when_prior_download_already_reserved_budget() {
        let temp_dir = std::env::temp_dir();
        let available = available_disk_bytes(temp_dir.as_path())
            .await
            .expect("supported platforms should report available disk space");
        let reserved_download_bytes = Arc::new(AtomicU64::new(0));
        let min_free_bytes = available.saturating_sub(1);

        let first = reserve_download_bytes(
            temp_dir.as_path(),
            min_free_bytes,
            1,
            &reserved_download_bytes,
        )
        .await
        .expect("first reservation should consume the remaining budget");

        assert_eq!(reserved_download_bytes.load(Ordering::Acquire), 1);

        let err = reserve_download_bytes(
            temp_dir.as_path(),
            min_free_bytes,
            1,
            &reserved_download_bytes,
        )
        .await
        .expect_err("second reservation should exceed the collective budget");

        assert!(matches!(err, Error::DiskSpace { .. }));

        drop(first);
        assert_eq!(reserved_download_bytes.load(Ordering::Acquire), 0);
    }
}
