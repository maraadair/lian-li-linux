use anyhow::{ensure, Context, Result};
use lianli_display::channel::{display_socket_path, PacketChannel, PacketListener};
use lianli_display::login::LoginMonitor;
use lianli_shared::display::{
    DisplayCodec, OutputRequest, WorkerClosed, WorkerCommand, WorkerHello, MAX_SESSION_DISPLAYS,
};
use lianli_shared::installation::InstallationContext;
use lianli_shared::session::DesktopSession;
use std::collections::HashSet;
use std::os::fd::{AsFd, AsRawFd};
use std::os::unix::net::UnixDatagram;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(super) struct CaptureConnection {
    pub channel: PacketChannel,
    pub permitted: Arc<AtomicBool>,
}

struct OpenRequest {
    output: OutputRequest,
    codec: DisplayCodec,
    reply: mpsc::SyncSender<Result<CaptureConnection>>,
}

#[derive(Clone)]
pub(super) struct SessionClient {
    requests: mpsc::SyncSender<OpenRequest>,
    wake: Arc<UnixDatagram>,
    ready: Arc<AtomicBool>,
    epoch: Arc<AtomicU64>,
}

impl SessionClient {
    pub fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Acquire)
    }
    pub fn ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    pub fn open(
        &self,
        output: OutputRequest,
        codec: DisplayCodec,
        stop: &AtomicBool,
    ) -> Result<CaptureConnection> {
        let (reply, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(OpenRequest {
                output,
                codec,
                reply,
            })
            .context("Display session request queue is full or unavailable")?;
        self.notify();
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            ensure!(!stop.load(Ordering::Relaxed), "Display startup cancelled");
            ensure!(
                Instant::now() < deadline,
                "Display session did not accept the request"
            );
            match result.recv_timeout(Duration::from_millis(50)) {
                Ok(result) => return result,
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn notify(&self) {
        if let Err(error) = self.wake.send(&[1]) {
            if error.kind() != std::io::ErrorKind::WouldBlock {
                tracing::debug!("Waking session coordinator failed: {error}");
            }
        }
    }
}

pub(super) struct SessionCoordinator {
    pub client: SessionClient,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl SessionCoordinator {
    pub fn start(ipc: &Path) -> Result<Self> {
        let (wake, receiver) = UnixDatagram::pair()?;
        wake.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let (requests, pending) = mpsc::sync_channel(8);
        let client = SessionClient {
            requests,
            wake: Arc::new(wake),
            ready: Arc::new(AtomicBool::new(false)),
            epoch: Arc::new(AtomicU64::new(0)),
        };
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let state = client.clone();
        let path = display_socket_path(ipc);
        let join = std::thread::Builder::new()
            .name("display-session".into())
            .spawn(move || {
                let context = InstallationContext::detect();
                let mut retry = 1;
                while !worker_stop.load(Ordering::Relaxed) {
                    let result = (|| {
                        let parent = path.parent().context("Display socket has no parent")?;
                        std::fs::create_dir_all(parent)?;
                        let listener = PacketListener::bind(path.clone())?;
                        let monitor = LoginMonitor::connect(&context)?;
                        coordinate(listener, monitor, &pending, &receiver, &state, &worker_stop)
                    })();
                    state.ready.store(false, Ordering::Release);
                    state.epoch.fetch_add(1, Ordering::AcqRel);
                    if worker_stop.load(Ordering::Relaxed) {
                        break;
                    }
                    if let Err(error) = result {
                        tracing::warn!(
                            "Desktop session unavailable: {error:#}; retrying in {retry}s"
                        );
                    }
                    let retry_at = Instant::now() + Duration::from_secs(retry);
                    while Instant::now() < retry_at && !worker_stop.load(Ordering::Relaxed) {
                        while let Ok(request) = pending.try_recv() {
                            let _ = request.reply.send(Err(anyhow::anyhow!(
                                "Login-session authorization is unavailable"
                            )));
                        }
                        poll(
                            &[(receiver.as_raw_fd(), libc::POLLIN)],
                            retry_at.saturating_duration_since(Instant::now()),
                        );
                        drain(&receiver);
                    }
                    retry = (retry * 2).min(30);
                }
            })?;
        Ok(Self {
            client,
            stop,
            join: Some(join),
        })
    }
}

impl Drop for SessionCoordinator {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.client.notify();
        if let Some(join) = self.join.take() {
            if join.join().is_err() {
                tracing::warn!("Display session coordinator panicked");
            }
        }
    }
}

struct Worker {
    channel: PacketChannel,
    uid: u32,
    session: String,
    permitted: Arc<AtomicBool>,
    displays: HashSet<u64>,
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.permitted.store(false, Ordering::Release);
    }
}

fn coordinate(
    listener: PacketListener,
    monitor: LoginMonitor,
    requests: &mpsc::Receiver<OpenRequest>,
    wake: &UnixDatagram,
    state: &SessionClient,
    stop: &AtomicBool,
) -> Result<()> {
    let mut worker: Option<Worker> = None;
    let mut session = None;
    let mut handshakes = Vec::new();
    let mut last_rejection: Option<String> = None;
    let mut next_id = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let events = monitor.process()?;
        if events.changed {
            let previously_permitted = worker
                .as_ref()
                .is_some_and(|worker| worker.permitted.load(Ordering::Acquire));
            if let Some(worker) = &worker {
                worker.permitted.store(false, Ordering::Release);
            }
            session = monitor.active_session()?;
            if worker.as_ref().is_some_and(|w| {
                !session
                    .as_ref()
                    .is_some_and(|s| s.matches_worker(w.uid, &w.session))
            }) {
                worker = None;
                state.epoch.fetch_add(1, Ordering::AcqRel);
            }
            if let Some(worker) = &worker {
                let permitted = session
                    .as_ref()
                    .is_some_and(|s| s.allows_capture(worker.uid, &worker.session));
                worker.permitted.store(permitted, Ordering::Release);
                if permitted && !previously_permitted {
                    state.epoch.fetch_add(1, Ordering::AcqRel);
                }
            }
        }
        state.ready.store(
            worker
                .as_ref()
                .is_some_and(|worker| worker.permitted.load(Ordering::Acquire)),
            Ordering::Release,
        );
        for _ in 0..4 {
            let Some(channel) = listener.accept()? else {
                break;
            };
            if handshakes.len() < 4 {
                handshakes.push((channel, Instant::now() + Duration::from_secs(1)));
            }
        }
        handshakes.retain_mut(|(channel, deadline)| {
            if Instant::now() >= *deadline {
                return false;
            }
            let hello = match channel.try_receive::<WorkerHello>() {
                Ok(Some(hello)) => hello,
                Ok(None) => return true,
                Err(_) => return false,
            };
            let Ok((uid, _)) = channel.peer_credentials() else {
                return false;
            };
            let rejection = worker_rejection(
                worker.is_some(),
                &hello.message,
                hello.descriptors.is_empty(),
                uid,
                session.as_ref(),
            );
            if let Some(reason) = rejection {
                if last_rejection.as_ref() != Some(&reason) {
                    tracing::warn!("Rejected desktop capture worker: {reason}");
                    last_rejection = Some(reason);
                }
                return false;
            }
            last_rejection = None;
            let Ok(owned) = channel.as_fd().try_clone_to_owned() else {
                return false;
            };
            let Ok(registered) = PacketChannel::new(owned) else {
                return false;
            };
            if registered
                .send(
                    &WorkerCommand::Registered,
                    &[],
                    Duration::from_millis(100),
                    stop,
                )
                .is_err()
            {
                return false;
            }
            worker = Some(Worker {
                channel: registered,
                uid,
                session: hello.message.session_id,
                permitted: Arc::new(AtomicBool::new(session.as_ref().is_some_and(|s| !s.locked))),
                displays: HashSet::new(),
            });
            state.epoch.fetch_add(1, Ordering::AcqRel);
            false
        });
        if let Some(current) = &mut worker {
            let mut disconnected = false;
            for _ in 0..MAX_SESSION_DISPLAYS {
                match current.channel.try_receive::<WorkerClosed>() {
                    Ok(Some(closed))
                        if closed.descriptors.is_empty()
                            && current.displays.remove(&closed.message.id) => {}
                    Ok(None) => break,
                    _ => {
                        disconnected = true;
                        break;
                    }
                }
            }
            if disconnected {
                worker = None;
                state.epoch.fetch_add(1, Ordering::AcqRel);
            }
        }
        for _ in 0..8 {
            let Ok(request) = requests.try_recv() else {
                break;
            };
            let result = (|| {
                let worker = worker
                    .as_mut()
                    .context("Waiting for an active desktop session worker")?;
                ensure!(
                    worker.permitted.load(Ordering::Acquire),
                    "Desktop session is locked or no longer active"
                );
                ensure!(
                    worker.displays.len() < MAX_SESSION_DISPLAYS,
                    "Session display capacity is exhausted"
                );
                request.output.validate()?;
                next_id = next_id
                    .checked_add(1)
                    .context("Display session ID exhausted")?;
                let (daemon, helper) = PacketChannel::pair()?;
                worker.channel.send(
                    &WorkerCommand::Open {
                        id: next_id,
                        output: request.output,
                        codec: request.codec,
                    },
                    &[helper.as_fd()],
                    Duration::from_millis(100),
                    stop,
                )?;
                worker.displays.insert(next_id);
                Ok(CaptureConnection {
                    channel: daemon,
                    permitted: worker.permitted.clone(),
                })
            })();
            // A cancelled requester closes its returned endpoint, which stops the helper job.
            let _ = request.reply.send(result);
        }
        state.ready.store(
            worker
                .as_ref()
                .is_some_and(|worker| worker.permitted.load(Ordering::Acquire)),
            Ordering::Release,
        );
        let (bus, events_mask) = monitor.poll_descriptor()?;
        let mut fds = vec![
            (listener.as_fd().as_raw_fd(), libc::POLLIN),
            (bus.as_raw_fd(), events_mask),
            (wake.as_raw_fd(), libc::POLLIN),
        ];
        if let Some(worker) = &worker {
            fds.push((worker.channel.as_fd().as_raw_fd(), libc::POLLIN));
        }
        for (channel, _) in &handshakes {
            fds.push((channel.as_fd().as_raw_fd(), libc::POLLIN));
        }
        let mut timeout = monitor
            .timeout()?
            .unwrap_or(Duration::from_secs(30))
            .min(Duration::from_secs(30));
        for (_, deadline) in &handshakes {
            timeout = timeout.min(deadline.saturating_duration_since(Instant::now()));
        }
        if events.pending {
            timeout = Duration::ZERO;
        }
        poll(&fds, timeout);
        drain(wake);
    }
    Ok(())
}

fn drain(socket: &UnixDatagram) {
    let mut bytes = [0; 32];
    while socket.recv(&mut bytes).is_ok() {}
}

fn poll(descriptors: &[(i32, i16)], timeout: Duration) {
    let mut fds: Vec<_> = descriptors
        .iter()
        .map(|(fd, events)| libc::pollfd {
            fd: *fd,
            events: *events,
            revents: 0,
        })
        .collect();
    let result = unsafe {
        libc::poll(
            fds.as_mut_ptr(),
            fds.len() as libc::nfds_t,
            timeout.as_millis().min(30_000) as i32,
        )
    };
    if result < 0 && std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
        // Avoid a hot loop if the process exhausts its polling resources.
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn worker_rejection(
    worker_registered: bool,
    hello: &WorkerHello,
    no_descriptors: bool,
    uid: u32,
    session: Option<&DesktopSession>,
) -> Option<String> {
    if worker_registered {
        return Some("another capture worker is already registered".into());
    }
    if !no_descriptors {
        return Some("the handshake carried unexpected file descriptors".into());
    }
    if hello.version != env!("CARGO_PKG_VERSION") {
        return Some(format!(
            "worker version {} does not match daemon version {}",
            hello.version,
            env!("CARGO_PKG_VERSION")
        ));
    }
    match session {
        None => Some(format!(
            "worker session {} is not the active graphical session; logind reports none",
            hello.session_id
        )),
        Some(active) if !active.matches_worker(uid, &hello.session_id) => Some(format!(
            "worker session {} (uid {uid}) does not match active session {} (uid {}, {:?})",
            hello.session_id, active.id, active.uid, active.kind
        )),
        Some(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lianli_shared::session::SessionKind;

    #[test]
    fn worker_rejections_name_the_failed_condition() {
        let hello = WorkerHello {
            session_id: "3".into(),
            version: env!("CARGO_PKG_VERSION").into(),
        };
        let active = DesktopSession {
            id: "3".into(),
            uid: 1000,
            kind: SessionKind::Wayland,
            locked: false,
        };
        assert!(worker_rejection(false, &hello, true, 1000, Some(&active)).is_none());
        assert!(worker_rejection(true, &hello, true, 1000, Some(&active))
            .unwrap()
            .contains("already registered"));
        assert!(worker_rejection(false, &hello, true, 1000, None)
            .unwrap()
            .contains("logind reports none"));
        assert!(worker_rejection(false, &hello, true, 1001, Some(&active))
            .unwrap()
            .contains("does not match active session"));
        let stale = WorkerHello {
            session_id: "3".into(),
            version: "0.0.0".into(),
        };
        assert!(worker_rejection(false, &stale, true, 1000, Some(&active))
            .unwrap()
            .contains("does not match daemon version"));
    }
}
