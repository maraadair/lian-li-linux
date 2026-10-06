use crate::{session_lock, wait_session, Cli};
use anyhow::{ensure, Context, Result};
use lianli_display::login::LoginMonitor;
use lianli_shared::installation::InstallationContext;
use lianli_shared::session::{DesktopSession, SessionKind};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub(super) const KEYS: [&str; 4] = [
    "XDG_SESSION_ID",
    "WAYLAND_DISPLAY",
    "HYPRLAND_INSTANCE_SIGNATURE",
    "XDG_CURRENT_DESKTOP",
];

pub fn run(
    cli: &Cli,
    monitor: &mut LoginMonitor,
    stop: &AtomicBool,
    context: &InstallationContext,
) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    ensure!(
        uid != 0,
        "Desktop login startup must run as the desktop user"
    );
    let runtime = std::path::PathBuf::from(format!("/run/user/{uid}"));
    if matches!(context, InstallationContext::Distrobox { .. }) {
        verify_shared_runtime(
            &runtime,
            &Path::new("/run/host").join(runtime.strip_prefix("/")?),
            uid,
        )?;
    }
    let executable = std::env::current_exe()?;
    let mut retry = Duration::from_secs(1);
    tracing::info!("Waiting for the active graphical login; no GUI is required");
    let mut last_wait = None;
    while !stop.load(Ordering::Relaxed) {
        let wait = match monitor.active_session()? {
            None => "logind reports no active local graphical session".to_owned(),
            Some(session) if session.uid != uid => {
                format!(
                    "the active login session {} belongs to uid {}",
                    session.id, session.uid
                )
            }
            Some(session) => match session_lock(&runtime, &session.id)? {
                None => format!(
                    "another lianli-session process already serves login session {}",
                    session.id
                ),
                Some(lock) => match discover(Path::new("/proc"), &runtime, &session)? {
                    Discovery::Missing(reason) => {
                        format!("login session {} was not discovered: {reason}", session.id)
                    }
                    Discovery::Found(environment) => {
                        drop(lock);
                        let mut command = Command::new(&executable);
                        for key in KEYS {
                            command.env_remove(key);
                        }
                        command
                            .envs(environment)
                            .env("XDG_RUNTIME_DIR", &runtime)
                            .env(
                                "DBUS_SESSION_BUS_ADDRESS",
                                format!("unix:path={}/bus", runtime.display()),
                            );
                        if let Some(socket) = &cli.socket {
                            command.arg("--socket").arg(socket);
                        }
                        if let Some(invocation) = &cli.service_invocation {
                            command.arg("--service-invocation").arg(invocation);
                        }
                        return Err(command.exec())
                            .context("Starting the discovered desktop session");
                    }
                },
            },
        };
        if last_wait.as_ref() != Some(&wait) {
            tracing::info!("Still waiting: {wait}");
            last_wait = Some(wait);
        }
        let deadline = Instant::now() + retry;
        retry = (retry * 2).min(Duration::from_secs(30));
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            if monitor.process()?.changed {
                retry = Duration::from_secs(1);
                break;
            }
            wait_session(
                monitor,
                None,
                deadline.saturating_duration_since(Instant::now()),
            )?;
        }
    }
    Ok(())
}

fn verify_shared_runtime(runtime: &Path, host_runtime: &Path, uid: u32) -> Result<()> {
    let guest = fs::metadata(runtime).context("Distrobox user runtime is unavailable")?;
    let host = fs::metadata(host_runtime)
        .context("Host user runtime is hidden; restore Distrobox runtime integration")?;
    ensure!(guest.is_dir() && host.is_dir() && guest.uid() == uid && host.uid() == uid
        && guest.mode() & 0o077 == 0 && host.mode() & 0o077 == 0
        && (guest.dev(), guest.ino()) == (host.dev(), host.ino()),
        "Distrobox and host must share the same private user runtime directory for automatic desktop startup; see the Distrobox guide");
    Ok(())
}

enum Discovery {
    Found(BTreeMap<String, String>),
    Missing(String),
}

fn discover(proc_root: &Path, runtime: &Path, session: &DesktopSession) -> Result<Discovery> {
    if session.kind == SessionKind::X11 {
        return Ok(Discovery::Found(BTreeMap::from([(
            "XDG_SESSION_ID".into(),
            session.id.clone(),
        )])));
    }
    let deadline = Instant::now() + Duration::from_millis(250);
    let mut remaining = 8 * 1024 * 1024usize;
    let mut selected: Option<BTreeMap<String, String>> = None;
    let mut hyprland_sessions = BTreeMap::new();
    let mut readable = 0usize;
    let mut matching = 0usize;
    for (count, entry) in fs::read_dir(proc_root)?.enumerate() {
        if count >= 16384 || Instant::now() >= deadline || remaining == 0 {
            return Ok(Discovery::Missing(
                "the process scan exceeded its time or size budget".into(),
            ));
        }
        let entry = entry?;
        if !entry
            .file_name()
            .as_encoded_bytes()
            .iter()
            .all(u8::is_ascii_digit)
        {
            continue;
        }
        let path = entry.path().join("environ");
        let Ok(file) = fs::File::open(path) else {
            continue;
        };
        if !file
            .metadata()
            .is_ok_and(|metadata| metadata.uid() == session.uid)
        {
            continue;
        }
        let mut bytes = Vec::new();
        let limit = remaining.min(64 * 1024 + 1);
        let read = file.take(limit as u64).read_to_end(&mut bytes);
        remaining = remaining.saturating_sub(bytes.len());
        if read.is_err() || bytes.len() >= limit {
            continue;
        }
        readable += 1;
        let Some(candidate) = parse(&bytes, session) else {
            continue;
        };
        matching += 1;
        let socket = runtime.join(&candidate["WAYLAND_DISPLAY"]);
        if !fs::symlink_metadata(socket)
            .is_ok_and(|metadata| metadata.uid() == session.uid && metadata.file_type().is_socket())
        {
            continue;
        }
        if let Some(signature) = candidate.get("HYPRLAND_INSTANCE_SIGNATURE") {
            let display = &candidate["WAYLAND_DISPLAY"];
            let usable = hyprland_sessions
                .entry((signature.clone(), display.clone()))
                .or_insert_with(|| {
                    lianli_display::hyprland::Control::verify_session(
                        runtime, signature, display, deadline,
                    )
                    .is_ok()
                });
            if !*usable {
                continue;
            }
        }
        if let Some(previous) = selected.as_ref().filter(|previous| *previous != &candidate) {
            return Ok(Discovery::Missing(format!(
                "processes disagree on the session environment: {} vs {}",
                describe(previous),
                describe(&candidate)
            )));
        }
        selected = Some(candidate);
    }
    if let Some(environment) = selected {
        return Ok(Discovery::Found(environment));
    }
    if let Some(socket) = sole_live_wayland_socket(runtime, session.uid) {
        tracing::info!(
            "No readable process carries the session environment; using the only live \
             compositor socket {socket}"
        );
        return Ok(Discovery::Found(BTreeMap::from([
            ("XDG_SESSION_ID".into(), session.id.clone()),
            ("WAYLAND_DISPLAY".into(), socket),
        ])));
    }
    let processes = if readable == 0 {
        format!(
            "no process environment owned by uid {} is readable",
            session.uid
        )
    } else if matching == 0 {
        format!(
            "no readable process has XDG_SESSION_ID={} with WAYLAND_DISPLAY",
            session.id
        )
    } else {
        "processes in this session name compositor sockets that are not live".into()
    };
    Ok(Discovery::Missing(format!(
        "{processes}, and {} has no single live wayland-N socket",
        runtime.display()
    )))
}

fn describe(environment: &BTreeMap<String, String>) -> String {
    KEYS.iter()
        .map(|key| {
            format!(
                "{key}={}",
                environment.get(*key).map_or("<unset>", String::as_str)
            )
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// Compositors with file capabilities, such as KWin on Fedora, have a root-owned
// environ. A single live socket is unambiguous; Hyprland is excluded because it
// also needs its instance signature.
fn sole_live_wayland_socket(runtime: &Path, uid: u32) -> Option<String> {
    if runtime.join("hypr").exists() {
        return None;
    }
    let mut found = None;
    for entry in fs::read_dir(runtime).ok()?.take(4096) {
        let entry = entry.ok()?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if !name
            .strip_prefix("wayland-")
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
        {
            continue;
        }
        let path = entry.path();
        if !fs::symlink_metadata(&path)
            .is_ok_and(|metadata| metadata.uid() == uid && metadata.file_type().is_socket())
            || !lianli_display::socket_accepts_connections(
                &path,
                Instant::now() + Duration::from_millis(100),
            )
        {
            continue;
        }
        if found.replace(name).is_some() {
            return None;
        }
    }
    found
}

fn parse(bytes: &[u8], session: &DesktopSession) -> Option<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for field in bytes.split(|byte| *byte == 0) {
        let Some(separator) = field.iter().position(|byte| *byte == b'=') else {
            continue;
        };
        let Ok(key) = std::str::from_utf8(&field[..separator]) else {
            continue;
        };
        if !KEYS.contains(&key) {
            continue;
        }
        let value = std::str::from_utf8(&field[separator + 1..]).ok()?;
        if value.len() > 256
            || value.chars().any(char::is_control)
            || environment
                .insert(key.to_owned(), value.to_owned())
                .is_some()
        {
            return None;
        }
    }
    if environment.get("XDG_SESSION_ID")? != &session.id {
        return None;
    }
    let display = environment.get("WAYLAND_DISPLAY")?;
    if display.is_empty() || display == "." || display == ".." || display.contains('/') {
        return None;
    }
    if let Some(signature) = environment.get("HYPRLAND_INSTANCE_SIGNATURE") {
        if signature.is_empty()
            || signature.len() > 128
            || !signature
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
        {
            return None;
        }
        environment.insert("XDG_CURRENT_DESKTOP".into(), "Hyprland".into());
    }
    Some(environment)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn found(discovery: Discovery) -> Option<BTreeMap<String, String>> {
        match discovery {
            Discovery::Found(environment) => Some(environment),
            Discovery::Missing(_) => None,
        }
    }

    #[test]
    fn distrobox_login_requires_the_same_owned_private_runtime() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(other.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let host_view = other.path().join("host-view");
        std::os::unix::fs::symlink(root.path(), &host_view).unwrap();
        let uid = unsafe { libc::geteuid() };
        verify_shared_runtime(root.path(), &host_view, uid).unwrap();
        assert!(verify_shared_runtime(root.path(), other.path(), uid).is_err());
        assert!(verify_shared_runtime(root.path(), &host_view, uid + 1).is_err());
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(verify_shared_runtime(root.path(), &host_view, uid).is_err());
    }

    #[test]
    fn discovery_requires_an_owned_live_socket_and_an_unambiguous_session() {
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(proc_root.join("123")).unwrap();
        fs::create_dir(&runtime).unwrap();
        fs::write(
            proc_root.join("123/environ"),
            b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-1\0",
        )
        .unwrap();
        let mut session = DesktopSession {
            id: "3".into(),
            uid: unsafe { libc::geteuid() },
            kind: SessionKind::Wayland,
            locked: false,
        };
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());
        let _socket = UnixListener::bind(runtime.join("wayland-1")).unwrap();
        assert_eq!(
            found(discover(&proc_root, &runtime, &session).unwrap()).unwrap()["WAYLAND_DISPLAY"],
            "wayland-1"
        );
        session.uid += 1;
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());
        session.uid -= 1;
        fs::create_dir(proc_root.join("456")).unwrap();
        fs::write(
            proc_root.join("456/environ"),
            b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-2\0",
        )
        .unwrap();
        let _other = UnixListener::bind(runtime.join("wayland-2")).unwrap();
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());
    }

    #[test]
    fn unreadable_compositor_falls_back_to_the_only_live_socket() {
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        fs::create_dir_all(&proc_root).unwrap();
        fs::create_dir(&runtime).unwrap();
        let session = DesktopSession {
            id: "3".into(),
            uid: unsafe { libc::geteuid() },
            kind: SessionKind::Wayland,
            locked: false,
        };
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());

        drop(UnixListener::bind(runtime.join("wayland-1")).unwrap());
        let _live = UnixListener::bind(runtime.join("wayland-0")).unwrap();
        fs::write(runtime.join("wayland-0.lock"), b"").unwrap();
        let environment = found(discover(&proc_root, &runtime, &session).unwrap()).unwrap();
        assert_eq!(environment["WAYLAND_DISPLAY"], "wayland-0");
        assert_eq!(environment["XDG_SESSION_ID"], "3");

        let second = UnixListener::bind(runtime.join("wayland-2")).unwrap();
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());
        drop(second);
        fs::remove_file(runtime.join("wayland-2")).unwrap();

        fs::create_dir(runtime.join("hypr")).unwrap();
        assert!(found(discover(&proc_root, &runtime, &session).unwrap()).is_none());
    }

    #[test]
    fn stale_hyprland_environments_do_not_hide_the_live_compositor() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixListener;
        let root = tempfile::tempdir().unwrap();
        let proc_root = root.path().join("proc");
        let runtime = root.path().join("runtime");
        for signature in ["old", "current"] {
            fs::create_dir_all(runtime.join("hypr").join(signature)).unwrap();
        }
        let _wayland = UnixListener::bind(runtime.join("wayland-0")).unwrap();
        let stale = UnixListener::bind(runtime.join("hypr/old/.socket.sock")).unwrap();
        drop(stale);
        let current = UnixListener::bind(runtime.join("hypr/current/.socket.sock")).unwrap();
        let server = std::thread::spawn(move || {
            let mut ready = libc::pollfd {
                fd: current.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut ready, 1, 2000) }, 1);
            let (mut client, _) = current.accept().unwrap();
            client
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            let mut request = [0; 9];
            client.read_exact(&mut request).unwrap();
            assert_eq!(&request, b"j/version");
            client.write_all(br#"{"version":"0.54.0"}"#).unwrap();
        });
        for (pid, signature) in [(123, "old"), (456, "current"), (789, "current")] {
            let process = proc_root.join(pid.to_string());
            fs::create_dir_all(&process).unwrap();
            fs::write(process.join("environ"), format!(
                "XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-0\0HYPRLAND_INSTANCE_SIGNATURE={signature}\0"
            )).unwrap();
        }
        let session = DesktopSession {
            id: "3".into(),
            uid: unsafe { libc::geteuid() },
            kind: SessionKind::Wayland,
            locked: false,
        };
        let environment = found(discover(&proc_root, &runtime, &session).unwrap()).unwrap();
        server.join().unwrap();
        assert_eq!(environment["HYPRLAND_INSTANCE_SIGNATURE"], "current");
    }

    #[test]
    fn login_environment_is_scoped_and_does_not_copy_unrelated_variables() {
        let session = DesktopSession {
            id: "3".into(),
            uid: 1000,
            kind: SessionKind::Wayland,
            locked: false,
        };
        let parsed = parse(b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-1\0HYPRLAND_INSTANCE_SIGNATURE=hash_123\0SECRET=private\0LD_PRELOAD=unsafe\0", &session).unwrap();
        assert_eq!(parsed.len(), 4);
        assert_eq!(parsed["XDG_CURRENT_DESKTOP"], "Hyprland");
        for input in [
            b"XDG_SESSION_ID=4\0WAYLAND_DISPLAY=wayland-1\0".as_slice(),
            b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=../other\0",
            b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-1\0WAYLAND_DISPLAY=wayland-2\0",
            b"XDG_SESSION_ID=3\0WAYLAND_DISPLAY=wayland-1\0HYPRLAND_INSTANCE_SIGNATURE=../other\0",
        ] {
            assert!(parse(input, &session).is_none());
        }
    }
}
