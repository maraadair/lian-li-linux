use crate::{wait_session, Cli};
use anyhow::{Context, Result};
use lianli_display::login::LoginMonitor;
use std::io;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub fn run(cli: &Cli, monitor: &mut LoginMonitor, session: &str, stop: &AtomicBool) -> Result<()> {
    let executable = std::env::current_exe().context("Locating the capture worker")?;
    let original = std::fs::metadata("/proc/self/exe")?;
    let compositor = std::env::var_os("WAYLAND_DISPLAY")
        .map(|display| {
            let path = PathBuf::from(display);
            if path.is_absolute() {
                Ok(path)
            } else {
                std::env::var_os("XDG_RUNTIME_DIR")
                    .map(|runtime| PathBuf::from(runtime).join(path))
                    .context("Wayland runtime directory is missing")
            }
        })
        .transpose()?
        .map(CompositorSocket::new)
        .transpose()?;
    tracing::info!("Supervising desktop capture for login session {session}");
    let mut next_upgrade_check = Instant::now();
    let mut delay = Duration::from_secs(1);
    while !stop.load(Ordering::Relaxed) && !monitor.process()?.session_ended {
        let mut command = Command::new(&executable);
        command
            .arg("--worker-session")
            .arg(session)
            .stdin(Stdio::null());
        if let Some(socket) = &cli.socket {
            command.arg("--socket").arg(socket);
        }
        let began = Instant::now();
        let mut upgrade = false;
        let mut rediscover = false;
        let result: Result<Option<ExitStatus>> = (|| {
            let mut child = OwnedChild::spawn(&mut command)?;
            loop {
                if stop.load(Ordering::Relaxed) || monitor.process()?.session_ended {
                    return Ok(None);
                }
                if Instant::now() >= next_upgrade_check {
                    next_upgrade_check = Instant::now() + Duration::from_secs(5);
                    if compositor.as_ref().is_some_and(CompositorSocket::changed) {
                        rediscover = true;
                        child.stop(Duration::from_secs(10))?;
                        return Ok(None);
                    }
                    if executable_replaced(&executable, &original) {
                        upgrade = true;
                        child.stop(Duration::from_secs(10))?;
                        return Ok(None);
                    }
                }
                if let Some(status) = child.try_wait()? {
                    return Ok(Some(status));
                }
                wait_session(monitor, None, Duration::from_secs(1))?;
            }
        })();
        if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
            if let Err(error) = lianli_display::hyprland::Control::from_env() {
                tracing::warn!("Hyprland output recovery after capture exit failed: {error:#}");
            }
        }
        if (upgrade || rediscover)
            && result.is_ok()
            && !stop.load(Ordering::Relaxed)
            && !monitor.process()?.session_ended
        {
            let error = if rediscover {
                tracing::info!(
                    "Compositor socket changed. Rediscovering the desktop after capture cleanup"
                );
                rediscovery_command(&executable, cli).exec()
            } else {
                tracing::info!("Session helper was upgraded; restarting after capture cleanup");
                Command::new(&executable)
                    .args(std::env::args_os().skip(1))
                    .exec()
            };
            return Err(error).context("Restarting the session helper");
        }
        if began.elapsed() >= Duration::from_secs(60) {
            delay = Duration::from_secs(1);
        }
        match result {
            Ok(None) => return Ok(()),
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => tracing::warn!(
                "Desktop capture exited with {status}; restarting in {}s",
                delay.as_secs()
            ),
            Err(error) => tracing::warn!(
                "Desktop capture failed: {error:#}; restarting in {}s",
                delay.as_secs()
            ),
        }
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            if monitor.process()?.session_ended {
                return Ok(());
            }
            wait_session(
                monitor,
                None,
                deadline.saturating_duration_since(Instant::now()),
            )?;
        }
        delay = (delay * 2).min(Duration::from_secs(30));
    }
    Ok(())
}

struct CompositorSocket {
    path: PathBuf,
    identity: (u64, u64, u32),
    _socket: std::fs::File,
}

impl CompositorSocket {
    fn new(path: PathBuf) -> Result<Self> {
        let socket = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(&path)
            .context("Wayland socket is unavailable")?;
        let metadata = socket.metadata()?;
        anyhow::ensure!(
            metadata.file_type().is_socket(),
            "Wayland endpoint is not a socket"
        );
        Ok(Self {
            path,
            identity: (metadata.dev(), metadata.ino(), metadata.uid()),
            _socket: socket,
        })
    }

    fn changed(&self) -> bool {
        !std::fs::symlink_metadata(&self.path).is_ok_and(|metadata| {
            metadata.file_type().is_socket()
                && (metadata.dev(), metadata.ino(), metadata.uid()) == self.identity
        })
    }
}

fn rediscovery_command(executable: &Path, cli: &Cli) -> Command {
    let mut command = Command::new(executable);
    command.arg("--login-start");
    if let Some(socket) = &cli.socket {
        command.arg("--socket").arg(socket);
    }
    if let Some(invocation) = &cli.service_invocation {
        command.arg("--service-invocation").arg(invocation);
    }
    for key in crate::login_start::KEYS {
        command.env_remove(key);
    }
    command
}

fn executable_replaced(path: &Path, original: &std::fs::Metadata) -> bool {
    std::fs::metadata(path).is_ok_and(|current| {
        current.is_file()
            && current.uid() == original.uid()
            && current.mode() & 0o111 != 0
            && (current.dev(), current.ino()) != (original.dev(), original.ino())
    })
}

struct OwnedChild(Option<Child>);

impl OwnedChild {
    fn spawn(command: &mut Command) -> Result<Self> {
        let parent = unsafe { libc::getpid() };
        // The capture process must release its DRM owners even if the supervisor is killed.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0
                    || libc::setpgid(0, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent {
                    return Err(io::Error::from_raw_os_error(libc::ECANCELED));
                }
                Ok(())
            });
        }
        Ok(Self(Some(
            command.spawn().context("Starting capture process")?,
        )))
    }

    fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        let Some(child) = self.0.as_mut() else {
            return Ok(None);
        };
        let status = child.try_wait()?;
        if status.is_some() {
            self.0 = None;
        }
        Ok(status)
    }

    fn stop(&mut self, grace: Duration) -> io::Result<Option<ExitStatus>> {
        let Some(child) = self.0.as_mut() else {
            return Ok(None);
        };
        if let Some(status) = child.try_wait()? {
            self.0 = None;
            return Ok(Some(status));
        }
        let group = -(child.id() as libc::pid_t);
        if unsafe { libc::kill(group, libc::SIGTERM) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let deadline = Instant::now() + grace;
        loop {
            if let Some(status) = child.try_wait()? {
                self.0 = None;
                return Ok(Some(status));
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        tracing::warn!("Capture process did not stop within its grace period; killing it");
        if unsafe { libc::kill(group, libc::SIGKILL) } != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        let status = child.wait()?;
        self.0 = None;
        Ok(Some(status))
    }
}

impl Drop for OwnedChild {
    fn drop(&mut self) {
        if let Err(error) = self.stop(Duration::from_secs(10)) {
            tracing::error!("Stopping desktop capture failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::AsRawFd;
    use std::os::unix::process::ExitStatusExt;

    #[test]
    fn compositor_socket_replacement_or_disappearance_requires_rediscovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("wayland-0");
        let _socket = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let watched = CompositorSocket::new(path.clone()).unwrap();
        assert!(!watched.changed());
        std::fs::remove_file(&path).unwrap();
        assert!(watched.changed());
        let _replacement = std::os::unix::net::UnixListener::bind(&path).unwrap();
        assert!(watched.changed());
        let replacement = CompositorSocket::new(path.clone()).unwrap();
        assert!(!replacement.changed());
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"not a compositor").unwrap();
        assert!(replacement.changed());
        assert!(CompositorSocket::new(path).is_err());
    }

    #[test]
    fn rediscovery_preserves_service_routing_and_clears_stale_desktop_environment() {
        let cli = Cli {
            socket: Some(PathBuf::from("/run/user/1000/custom.sock")),
            service_invocation: Some("a".repeat(32)),
            stop_service: None,
            worker_session: None,
            login_start: false,
        };
        let command = rediscovery_command(Path::new("/usr/bin/lianli-session"), &cli);
        let arguments: Vec<_> = command
            .get_args()
            .map(|value| value.to_str().unwrap())
            .collect();
        assert_eq!(
            arguments,
            [
                "--login-start",
                "--socket",
                "/run/user/1000/custom.sock",
                "--service-invocation",
                &"a".repeat(32)
            ]
        );
        let environment: std::collections::BTreeMap<_, _> = command.get_envs().collect();
        for key in crate::login_start::KEYS {
            assert_eq!(environment.get(std::ffi::OsStr::new(key)), Some(&None));
        }
    }

    #[test]
    fn executable_upgrade_requires_a_replaced_executable_file() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("helper");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let original = std::fs::metadata(&path).unwrap();
        assert!(!executable_replaced(&path, &original));
        let replacement = directory.path().join("new");
        std::fs::write(&replacement, "new").unwrap();
        std::fs::set_permissions(&replacement, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        assert!(!executable_replaced(&path, &original));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(executable_replaced(&path, &original));
        std::fs::remove_file(&path).unwrap();
        assert!(!executable_replaced(&path, &original));
    }

    fn ready_child(script: &str) -> OwnedChild {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", script]).stdout(Stdio::piped());
        let mut child = OwnedChild::spawn(&mut command).unwrap();
        let mut ready_fd = libc::pollfd {
            fd: child
                .0
                .as_ref()
                .unwrap()
                .stdout
                .as_ref()
                .unwrap()
                .as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(unsafe { libc::poll(&mut ready_fd, 1, 3000) }, 1);
        let mut ready = [0; 5];
        child
            .0
            .as_mut()
            .unwrap()
            .stdout
            .as_mut()
            .unwrap()
            .read_exact(&mut ready)
            .unwrap();
        assert_eq!(&ready, b"ready");
        child
    }

    #[test]
    fn cooperative_child_stops_without_waiting_out_the_grace_period() {
        let mut child = ready_child("printf ready; exec sleep 30");
        let began = Instant::now();
        let status = child.stop(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
        assert!(began.elapsed() < Duration::from_secs(3));
    }

    #[test]
    fn unresponsive_child_is_killed_and_reaped_after_the_grace_period() {
        let mut child = ready_child("trap '' TERM; printf ready; exec sleep 30");
        let pid = child.0.as_ref().unwrap().id() as libc::pid_t;
        let began = Instant::now();
        let status = child.stop(Duration::from_millis(100)).unwrap().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(began.elapsed() < Duration::from_secs(3));
        assert_eq!(
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }
}
