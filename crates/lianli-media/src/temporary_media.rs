use crate::MediaError;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use tempfile::TempDir;

const MAX_TEMPORARY_BYTES: u64 = 2 * 1024 * 1024 * 1024;
static TEMPORARY_BYTES: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct TemporaryMedia {
    directory: Option<TempDir>,
    bytes: u64,
    used: &'static AtomicU64,
}

impl TemporaryMedia {
    pub(crate) fn new(bytes: u64) -> Result<Self, MediaError> {
        Self::reserve(bytes, &TEMPORARY_BYTES, MAX_TEMPORARY_BYTES)
    }

    fn reserve(bytes: u64, used: &'static AtomicU64, limit: u64) -> Result<Self, MediaError> {
        used.try_update(Ordering::Relaxed, Ordering::Relaxed, |current| {
            current.checked_add(bytes).filter(|total| *total <= limit)
        }).map_err(|_| MediaError::InvalidConfig(
            "H.264 preparation exceeds the shared 2 GiB temporary-storage limit. Release unused media before retrying.".into()
        ))?;
        let mut reserved = Self {
            directory: None,
            bytes,
            used,
        };
        reserved.directory = Some(TempDir::new()?);
        Ok(reserved)
    }

    pub fn path(&self) -> &Path {
        self.directory
            .as_ref()
            .expect("temporary media directory is live")
            .path()
    }

    pub(crate) fn retain_bytes(&mut self, bytes: u64) -> Result<(), MediaError> {
        if bytes > self.bytes {
            return Err(MediaError::InvalidConfig(
                "Prepared media exceeds its reserved temporary storage".into(),
            ));
        }
        self.used.fetch_sub(self.bytes - bytes, Ordering::Relaxed);
        self.bytes = bytes;
        Ok(())
    }
}

impl Drop for TemporaryMedia {
    fn drop(&mut self) {
        if let Some(directory) = self.directory.take() {
            let path = directory.path().to_path_buf();
            if let Err(error) = directory.close() {
                tracing::warn!(
                    path = %path.display(),
                    "Cannot remove temporary media; retaining its storage reservation: {error}"
                );
                return;
            }
        }
        self.used.fetch_sub(self.bytes, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn failed_directory_cleanup_does_not_release_storage_capacity() {
        static USED: AtomicU64 = AtomicU64::new(0);
        let media = TemporaryMedia::reserve(40, &USED, 100).unwrap();
        let path = media.path().to_path_buf();
        let root = tempfile::tempdir().unwrap();
        std::fs::rename(&path, root.path().join("moved")).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(media);
        assert_eq!(USED.load(Ordering::Relaxed), 40);
        assert!(TemporaryMedia::reserve(61, &USED, 100).is_err());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn storage_remains_reserved_until_files_and_their_last_owner_are_released() {
        static USED: AtomicU64 = AtomicU64::new(0);
        let mut first = TemporaryMedia::reserve(80, &USED, 100).unwrap();
        let file = first.path().join("stream.h264");
        std::fs::write(&file, [7u8; 40]).unwrap();
        first.retain_bytes(40).unwrap();
        let first = Arc::new(first);
        let reader = Arc::clone(&first);
        let next = TemporaryMedia::reserve(60, &USED, 100).unwrap();
        assert!(TemporaryMedia::reserve(1, &USED, 100).is_err());
        drop(first);
        assert_eq!(std::fs::read(&file).unwrap(), [7u8; 40]);
        assert!(TemporaryMedia::reserve(1, &USED, 100).is_err());
        drop(reader);
        assert!(!file.exists());
        assert_eq!(USED.load(Ordering::Relaxed), 60);
        drop(next);
        assert_eq!(USED.load(Ordering::Relaxed), 0);
    }
}
