mod capture;
mod encoding;
mod login_start;
mod service_control;
mod supervisor;
mod watchdog;

use anyhow::{ensure, Context, Result};
use clap::Parser;
use lianli_display::channel::{display_socket_path, PacketChannel};
use lianli_display::login::LoginMonitor;
use lianli_shared::display::{WorkerClosed, WorkerCommand, WorkerHello, MAX_SESSION_DISPLAYS};
use lianli_shared::installation::InstallationContext;
use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Parser)]
#[command(
    version,
    about = "Session-side desktop display capture for Lian Li Linux"
)]
struct Cli {
    #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation, conflicts_with_all = ["stop_service", "worker_session"])]
    service_invocation: Option<String>,
    /// Stop only the capture helper registered for this service invocation.
    #[arg(long, value_parser = lianli_shared::daemon::parse_service_invocation, conflicts_with_all = ["service_invocation", "login_start", "worker_session", "socket"])]
    stop_service: Option<String>,
    /// Daemon IPC socket; the display channel suffix is added automatically
    #[arg(long)]
    socket: Option<PathBuf>,
    #[arg(long, hide = true)]
    worker_session: Option<String>,
    /// Wait for a graphical login and discover its desktop environment
    #[arg(long, conflicts_with = "worker_session")]
    login_start: bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Some(invocation) = &cli.stop_service {
        return service_control::stop(invocation);
    }
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();
    let stop = Arc::new(AtomicBool::new(false));
    signal_hook::flag::register(signal_hook::consts::SIGTERM, stop.clone())?;
    signal_hook::flag::register(signal_hook::consts::SIGINT, stop.clone())?;
    let _service = cli
        .service_invocation
        .as_deref()
        .map(service_control::Guard::start)
        .transpose()?;
    let context = InstallationContext::detect();
    let mut monitor = LoginMonitor::connect(&context)?;
    if cli.login_start {
        return login_start::run(&cli, &mut monitor, &stop, &context);
    }
    let session = monitor
        .active_session()?
        .context("No active local graphical session")?;
    ensure!(
        session.uid == unsafe { libc::geteuid() },
        "The active desktop belongs to another user"
    );
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is missing")?;
    if let Some(expected) = &cli.worker_session {
        ensure!(
            expected == &session.id,
            "The worker's login session is no longer active"
        );
    }
    let lock_id = if cli.worker_session.is_some() {
        format!("{}_capture", session.id)
    } else {
        session.id.clone()
    };
    let Some(_singleton) = session_lock(&runtime, &lock_id)? else {
        tracing::info!(
            "Another lianli-session process already serves login session {}; exiting",
            session.id
        );
        return Ok(());
    };
    monitor.watch_session(&session.id)?;
    if cli.worker_session.is_none() {
        return supervisor::run(&cli, &mut monitor, &session.id, &stop);
    }
    let mut delay = 1;
    let mut last_error = None;
    while !stop.load(Ordering::Relaxed) {
        if monitor.process()?.session_ended {
            break;
        }
        let began = Instant::now();
        if let Err(error) = connect_and_run(&cli, &monitor, &context, &session.id, &stop) {
            let message = format!("{error:#}");
            if !stop.load(Ordering::Relaxed) && last_error.as_ref() != Some(&message) {
                tracing::warn!("Desktop worker waiting: {message}; retrying with bounded backoff");
            }
            last_error = Some(message);
        }
        if began.elapsed() > Duration::from_secs(60) {
            delay = 1;
        }
        let deadline = Instant::now() + Duration::from_secs(delay);
        while !stop.load(Ordering::Relaxed) && Instant::now() < deadline {
            if monitor.process()?.session_ended {
                return Ok(());
            }
            wait_session(
                &monitor,
                None,
                deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(1)),
            )?;
        }
        delay = (delay * 2).min(30);
    }
    Ok(())
}

fn connect_and_run(
    cli: &Cli,
    monitor: &LoginMonitor,
    context: &InstallationContext,
    expected_session: &str,
    stop: &AtomicBool,
) -> Result<()> {
    let session = monitor
        .active_session()?
        .context("No active local graphical session")?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        session.uid == uid,
        "The active desktop belongs to another user"
    );
    ensure!(
        session.id == expected_session,
        "Waiting for the worker's login session to become active"
    );
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        ensure!(
            id == session.id,
            "The worker environment belongs to another login session"
        );
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .context("XDG_RUNTIME_DIR is missing")?;
    ensure!(runtime.is_absolute(), "XDG_RUNTIME_DIR must be absolute");
    let system = context.system_socket_path();
    let candidates = daemon_candidates(cli.socket.as_deref(), &runtime, context);
    let mut last_error = None;
    for path in candidates {
        let result = (|| {
            let mut channel =
                PacketChannel::connect(&display_socket_path(&path), Duration::from_secs(1))?;
            let expected = if path == system {
                lianli_control::service_selection::system_peer_uid(context)?
            } else {
                uid
            };
            ensure!(
                channel.peer_credentials()?.0 == expected,
                "Unexpected hardware-daemon account"
            );
            channel.send(
                &WorkerHello {
                    session_id: session.id.clone(),
                    version: env!("CARGO_PKG_VERSION").into(),
                },
                &[],
                Duration::from_secs(1),
                stop,
            )?;
            let accepted = channel.receive::<WorkerCommand>(Duration::from_secs(2), stop)?;
            ensure!(
                accepted.descriptors.is_empty()
                    && matches!(accepted.message, WorkerCommand::Registered),
                "Daemon rejected the desktop session"
            );
            Ok(channel)
        })();
        match result {
            Ok(channel) => return serve(channel, monitor, stop),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("No daemon socket available")))
}

fn daemon_candidates(
    socket: Option<&Path>,
    runtime: &Path,
    context: &InstallationContext,
) -> Vec<PathBuf> {
    if let Some(socket) = socket {
        return vec![socket.to_path_buf()];
    }
    let mut paths = vec![runtime.join("lianli-daemon.sock")];
    if !matches!(context, InstallationContext::UnsupportedContainer) {
        paths.push(context.system_socket_path());
    }
    paths
}

fn session_lock(runtime: &Path, id: &str) -> Result<Option<File>> {
    ensure!(
        !id.is_empty()
            && id.len() <= 128
            && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
        "Invalid login-session identifier"
    );
    let metadata = std::fs::symlink_metadata(runtime)?;
    let uid = unsafe { libc::geteuid() };
    ensure!(
        runtime.is_absolute()
            && metadata.is_dir()
            && metadata.uid() == uid
            && metadata.mode() & 0o077 == 0,
        "Session runtime directory must be private and owned by the current user"
    );
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(runtime.join(format!("lianli-session-{id}.lock")))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == uid && metadata.nlink() == 1,
        "Invalid session-worker lock object"
    );
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::WouldBlock {
            return Ok(None);
        }
        return Err(error.into());
    }
    Ok(Some(file))
}

struct Job {
    stop: Arc<AtomicBool>,
    thread: JoinHandle<()>,
    connection: OwnedFd,
    watch: Arc<watchdog::Watchdog>,
    cancelled_at: Option<Instant>,
}

#[derive(Default)]
struct Jobs(HashMap<u64, Job>);

impl Drop for Jobs {
    fn drop(&mut self) {
        for job in self.0.values() {
            job.stop.store(true, Ordering::Release);
        }
        let deadline = Instant::now() + watchdog::STOP_GRACE;
        while self.0.values().any(|job| !job.thread.is_finished()) {
            if Instant::now() >= deadline {
                watchdog::restart_capture_process("capture did not stop within ten seconds");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        for (_, job) in self.0.drain() {
            if job.thread.join().is_err() {
                tracing::warn!("Session capture worker panicked during shutdown");
            }
        }
    }
}

fn serve(mut channel: PacketChannel, monitor: &LoginMonitor, stop: &AtomicBool) -> Result<()> {
    let mut jobs = Jobs::default();
    tracing::info!("Desktop worker registered with the hardware daemon");
    while !stop.load(Ordering::Relaxed) {
        if monitor.process()?.session_ended {
            return Ok(());
        }
        for job in jobs.0.values_mut().filter(|job| !job.thread.is_finished()) {
            if peer_closed(job.connection.as_fd()) {
                job.stop.store(true, Ordering::Release);
                let cancelled = job.cancelled_at.get_or_insert_with(Instant::now);
                if cancelled.elapsed() >= watchdog::STOP_GRACE {
                    watchdog::restart_capture_process(
                        "abandoned capture did not release its resources",
                    );
                }
            }
            if job.watch.stalled(Instant::now()) {
                watchdog::restart_capture_process(
                    "a capture operation made no progress for twenty seconds",
                );
            }
        }
        let finished: Vec<_> = jobs
            .0
            .iter()
            .filter_map(|(id, job)| job.thread.is_finished().then_some(*id))
            .collect();
        for id in finished {
            if jobs.0.remove(&id).unwrap().thread.join().is_err() {
                tracing::warn!("Session capture worker panicked");
            }
            channel.send(&WorkerClosed { id }, &[], Duration::from_millis(100), stop)?;
        }
        if let Some(mut command) = channel.try_receive::<WorkerCommand>()? {
            match command.message {
                WorkerCommand::Registered => anyhow::bail!("Duplicate session registration"),
                WorkerCommand::Open { id, output, codec } => {
                    ensure!(
                        command.descriptors.len() == 1
                            && jobs.0.len() < MAX_SESSION_DISPLAYS
                            && !jobs.0.contains_key(&id),
                        "Invalid capture-worker request"
                    );
                    output.validate()?;
                    let peer = PacketChannel::new(command.descriptors.pop().unwrap())?;
                    let connection = peer.as_fd().try_clone_to_owned()?;
                    let stop = Arc::new(AtomicBool::new(false));
                    let capture_stop = stop.clone();
                    let watch = Arc::new(watchdog::Watchdog::new());
                    let capture_watch = watch.clone();
                    let thread = std::thread::Builder::new()
                        .name(format!("desktop-{id}"))
                        .spawn(move || {
                            capture::run(peer, output, codec, &capture_stop, &capture_watch)
                        })?;
                    jobs.0.insert(
                        id,
                        Job {
                            stop,
                            thread,
                            connection,
                            watch,
                            cancelled_at: None,
                        },
                    );
                }
            }
        }
        wait_session(
            monitor,
            Some(&channel),
            if jobs.0.is_empty() {
                Duration::from_secs(1)
            } else {
                Duration::from_millis(200)
            },
        )?;
    }
    Ok(())
}

fn peer_closed(connection: BorrowedFd<'_>) -> bool {
    let mut fd = libc::pollfd {
        fd: connection.as_raw_fd(),
        events: 0,
        revents: 0,
    };
    (unsafe { libc::poll(&mut fd, 1, 0) }) > 0
        && fd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0
}

fn wait_session(
    monitor: &LoginMonitor,
    channel: Option<&PacketChannel>,
    timeout: Duration,
) -> Result<()> {
    let (bus, events) = monitor.poll_descriptor()?;
    let mut fds = vec![libc::pollfd {
        fd: bus.as_raw_fd(),
        events,
        revents: 0,
    }];
    if let Some(channel) = channel {
        fds.push(libc::pollfd {
            fd: channel.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
    }
    let timeout = monitor
        .timeout()?
        .map_or(timeout, |deadline| timeout.min(deadline));
    let result = unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            timeout.as_millis().min(1000) as i32,
        )
    };
    if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn capture_discovers_the_host_system_socket_from_inside_the_box() {
        let runtime = Path::new("/run/user/1000");
        let boxed = InstallationContext::Distrobox {
            name: "fixture".into(),
        };
        assert_eq!(
            daemon_candidates(None, runtime, &boxed),
            vec![
                runtime.join("lianli-daemon.sock"),
                PathBuf::from("/run/host/run/lianli/lianli-daemon.sock"),
            ]
        );
        assert_eq!(
            daemon_candidates(None, runtime, &InstallationContext::Native)[1],
            PathBuf::from("/run/lianli/lianli-daemon.sock")
        );
        assert_eq!(
            daemon_candidates(Some(Path::new("/explicit.sock")), runtime, &boxed),
            vec![PathBuf::from("/explicit.sock")]
        );
        assert_eq!(
            daemon_candidates(None, runtime, &InstallationContext::UnsupportedContainer),
            vec![runtime.join("lianli-daemon.sock")]
        );
    }

    #[test]
    fn abandoned_display_startup_can_cancel_before_the_backend_finishes() {
        let (daemon, helper) = PacketChannel::pair().unwrap();
        assert!(!peer_closed(helper.as_fd()));
        drop(daemon);
        let deadline = Instant::now() + Duration::from_secs(3);
        while !peer_closed(helper.as_fd()) {
            assert!(
                Instant::now() < deadline,
                "Abandoned display endpoint remains open"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn singleton_is_scoped_to_the_login_and_rejects_unsafe_lock_objects() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = session_lock(dir.path(), "2").unwrap().unwrap();
        assert!(session_lock(dir.path(), "2").unwrap().is_none());
        assert!(session_lock(dir.path(), "3").unwrap().is_some());
        drop(owner);
        // Parallel subprocess tests retain inherited descriptors briefly between fork and exec.
        let deadline = Instant::now() + Duration::from_secs(3);
        while session_lock(dir.path(), "2").unwrap().is_none() {
            assert!(
                Instant::now() < deadline,
                "Released session lock remains held"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session_lock(dir.path(), "../2").is_err());
        std::os::unix::fs::symlink(
            "lianli-session-2.lock",
            dir.path().join("lianli-session-4.lock"),
        )
        .unwrap();
        assert!(session_lock(dir.path(), "4").is_err());
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(session_lock(dir.path(), "2").is_err());
    }
}
