use crate::ownership::open_identity;
use crate::reservation::HardwareReservation;
use anyhow::{ensure, Context, Result};
use lianli_shared::daemon::FileIdentity;
use lianli_shared::installation::InstallationContext;
use lianli_shared::ipc::IpcRequest;
use std::fs::File;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

const MAX_PENDING_WRITES: usize = 64;

pub struct ServiceWriteGate {
    path: Option<PathBuf>,
    require_root: bool,
    identity: OnceLock<(File, FileIdentity)>,
    pending: Arc<AtomicUsize>,
}

#[must_use = "Keep the permit alive until the settings write finishes"]
pub struct ServiceWritePermit {
    _lock: HardwareReservation,
    _slot: PendingWrite,
}

struct PendingWrite(Arc<AtomicUsize>, usize);

impl Drop for PendingWrite {
    fn drop(&mut self) {
        self.0.fetch_sub(self.1, Ordering::Release);
    }
}

impl std::fmt::Debug for ServiceWritePermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ServiceWritePermit")
            .finish_non_exhaustive()
    }
}

impl ServiceWriteGate {
    pub fn from_path(path: PathBuf, require_root: bool) -> Self {
        Self {
            path: Some(path),
            require_root,
            identity: OnceLock::new(),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn new(context: &InstallationContext) -> Self {
        Self {
            path: context.service_operation_lock_path(),
            require_root: matches!(context, InstallationContext::Native),
            identity: OnceLock::new(),
            pending: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn identity(&self) -> Result<FileIdentity> {
        let path = self
            .path
            .as_ref()
            .context("Host service write lock is unavailable in this container")?;
        let (file, current) = open_identity(path, self.require_root)
            .context("Service write lock is unavailable. Install the current host tmpfiles rule and run sudo systemd-tmpfiles --create lianli.conf before changing settings. See the Service modes guide in Installation Health")?;
        let (_, pinned) = self.identity.get_or_init(|| (file, current.clone()));
        ensure!(*pinned == current, "Service write lock was replaced while this daemon was running. Stop the daemon cleanly and repair the host setup before restarting it");
        Ok(pinned.clone())
    }

    pub fn guard(&self, request: &IpcRequest) -> Result<Option<ServiceWritePermit>> {
        if request.is_read_only() || matches!(request, IpcRequest::StopService { .. }) {
            return Ok(None);
        }
        let slots = if matches!(
            request,
            IpcRequest::RestoreStateBackup { .. }
                | IpcRequest::StartCatalogRemoval { .. }
                | IpcRequest::StartManagedMediaRemoval { .. }
        ) {
            MAX_PENDING_WRITES
        } else {
            1
        };
        self.pending
            .try_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count + slots <= MAX_PENDING_WRITES).then_some(count + slots)
            })
            .map_err(|_| {
                anyhow::anyhow!(
                    "Too many pending settings changes. Wait for them to finish, then retry"
                )
            })?;
        let slot = PendingWrite(self.pending.clone(), slots);
        let identity = self.identity()?;
        let path = self
            .path
            .as_ref()
            .context("Host service write lock is unavailable")?;
        let lock = HardwareReservation::shared_control(path.clone(), &identity, self.require_root)
            .context("Settings changes are paused while a service operation is running. Wait for it to finish, then retry your change")?;
        Ok(Some(ServiceWritePermit {
            _lock: lock,
            _slot: slot,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::os::fd::AsRawFd;

    fn gate(path: PathBuf) -> ServiceWriteGate {
        ServiceWriteGate::from_path(path, false)
    }

    fn write() -> IpcRequest {
        IpcRequest::SetLcdTemplates { templates: vec![] }
    }

    #[test]
    fn restore_excludes_pending_saves_and_keeps_read_only_requests_available() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control");
        std::fs::write(&path, "").unwrap();
        let gate = gate(path);
        let restore = IpcRequest::RestoreStateBackup {
            target: lianli_shared::backups::BackupTarget::Configuration,
            sha256: "reviewed".into(),
        };
        let save = gate.guard(&write()).unwrap();
        assert!(gate.guard(&restore).is_err());
        let removal = IpcRequest::StartCatalogRemoval {
            operation_id: "reviewed".into(),
        };
        assert!(gate.guard(&removal).is_err());
        drop(save);
        let restoring = gate.guard(&restore).unwrap();
        assert!(gate.guard(&write()).is_err());
        assert!(gate.guard(&restore).is_err());
        assert!(gate.guard(&IpcRequest::GetConfig).unwrap().is_none());
        drop(restoring);
        let removing = gate.guard(&removal).unwrap();
        assert!(gate.guard(&write()).is_err());
        assert!(gate.guard(&restore).is_err());
        assert!(gate.guard(&removal).is_err());
        assert!(gate
            .guard(&IpcRequest::GetCatalogReview {
                operation_id: "reviewed".into()
            })
            .unwrap()
            .is_none());
        drop(removing);
        let managed = IpcRequest::StartManagedMediaRemoval {
            operation_id: "reviewed".into(),
        };
        let removing = gate.guard(&managed).unwrap();
        assert!(gate.guard(&write()).is_err());
        assert!(gate.guard(&managed).is_err());
        assert!(gate.guard(&removal).is_err());
        assert!(gate
            .guard(&IpcRequest::GetManagedMediaReview {
                operation_id: "reviewed".into()
            })
            .unwrap()
            .is_none());
        drop(removing);
        assert!(gate.guard(&write()).is_ok());
    }

    #[test]
    fn pending_writes_are_bounded_and_completion_frees_capacity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control");
        std::fs::write(&path, "").unwrap();
        let gate = gate(path);
        let mut permits = Vec::new();
        for _ in 0..64 {
            permits.push(gate.guard(&write()).unwrap().unwrap());
        }
        assert!(gate.guard(&write()).is_err());
        assert!(gate.guard(&IpcRequest::GetConfig).unwrap().is_none());
        permits.pop();
        assert!(gate.guard(&write()).unwrap().is_some());
        drop(permits);
        assert_eq!(gate.pending.load(Ordering::Acquire), 0);
    }

    #[test]
    fn service_operations_and_multiple_settings_writers_exclude_each_other() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control");
        std::fs::write(&path, "preserve").unwrap();
        let gate = gate(path.clone());
        let operation = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        let first = gate.guard(&write()).unwrap().unwrap();
        let second = gate.guard(&write()).unwrap().unwrap();
        let reserve =
            || unsafe { libc::flock(operation.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        assert_ne!(reserve(), 0);
        drop(first);
        assert_ne!(reserve(), 0);
        drop(second);
        assert_eq!(reserve(), 0);
        assert!(gate.guard(&write()).is_err());
        assert!(gate.guard(&IpcRequest::GetConfig).unwrap().is_none());
        assert!(gate
            .guard(&IpcRequest::StopService {
                invocation_id: "test".into()
            })
            .unwrap()
            .is_none());
        assert_eq!(
            unsafe { libc::flock(operation.as_raw_fd(), libc::LOCK_UN) },
            0
        );
        assert!(gate.guard(&write()).unwrap().is_some());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "preserve");
    }

    #[test]
    fn missing_setup_can_be_repaired_but_replacing_a_pinned_lock_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("control");
        let gate = gate(path.clone());
        assert!(gate.guard(&IpcRequest::GetConfig).unwrap().is_none());
        for _ in 0..64 {
            assert!(gate.guard(&write()).is_err());
        }
        assert!(!path.exists());
        std::fs::write(&path, "").unwrap();
        let permit = gate.guard(&write()).unwrap();
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::write(&path, "").unwrap();
        assert!(gate.identity().is_err());
        assert!(gate.guard(&write()).is_err());
        drop(permit);
        assert!(gate.guard(&write()).is_err());
    }
}
