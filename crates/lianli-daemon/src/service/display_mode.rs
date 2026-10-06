use super::runtime::{ActiveTarget, LcdBackend};
use super::ServiceManager;
use anyhow::Context;
use lianli_devices::winusb::lcd::WinUsbLcdDevice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tracing::{info, warn};

const REFRESH_INTERVAL: Duration = Duration::from_secs(3);
const REFRESH_WINDOW: Duration = Duration::from_secs(9);

pub(super) struct DisplaySwitch {
    device_id: String,
    key: Option<crate::desktop_display::DeviceKey>,
    destination: &'static str,
    selected: Option<lianli_devices::detect::DetectedDevice>,
    task: Option<thread::JoinHandle<anyhow::Result<()>>>,
    stop: Arc<AtomicBool>,
    deadline: Instant,
}

impl Drop for DisplaySwitch {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            if task.join().is_err() {
                warn!("Display mode switch worker panicked during shutdown");
            }
        }
    }
}

#[derive(Default)]
pub(super) struct PostSwitchRefresh(Option<RefreshWindow>);

struct RefreshWindow {
    next: Instant,
    until: Instant,
}

impl PostSwitchRefresh {
    fn schedule(&mut self, now: Instant) {
        match &mut self.0 {
            Some(window) => window.until = now + REFRESH_WINDOW,
            None => {
                self.0 = Some(RefreshWindow {
                    next: now + REFRESH_INTERVAL,
                    until: now + REFRESH_WINDOW,
                })
            }
        }
    }

    fn is_idle(&self) -> bool {
        self.0.is_none()
    }

    fn take_due(&mut self, now: Instant) -> bool {
        let Some(window) = &mut self.0 else {
            return false;
        };
        if now < window.next {
            return false;
        }
        if now >= window.until {
            self.0 = None;
        } else {
            window.next = (now + REFRESH_INTERVAL).min(window.until);
        }
        true
    }
}

fn report_stale_usb_enumeration() {
    let missing = lianli_devices::detect::known_devices_missing_from_libusb();
    if !missing.is_empty() {
        warn!(
            "USB devices listed by the kernel are missing from libusb after a mode switch: {}. \
             USB hotplug events are not reaching the daemon, which happens inside some \
             containers; restart the daemon to rescan",
            missing.join(", ")
        );
    }
}

impl ServiceManager {
    pub(super) fn handle_display_switch_to_desktop(&mut self, device_id: &str) {
        if self.startup_image_job.is_some() {
            warn!("Startup image upload is still running");
            return;
        }
        if self.display_switch.is_some() {
            warn!("A display mode switch is already running");
            return;
        }
        if !self.force_stop_pixel_cleaning(Some(device_id.to_string())) {
            warn!("LCD targets are busy; retry switching {device_id} to desktop mode");
            return;
        }
        let Some(mut targets) = self.targets.try_lock_for(Duration::from_millis(10)) else {
            warn!("LCD targets are busy. Retry switching {device_id} to desktop mode");
            return;
        };
        let target_idx = targets
            .iter()
            .find_map(|(&idx, target)| (target.device_identity == device_id).then_some(idx));
        if target_idx.is_some_and(|idx| !matches!(targets[&idx].lcd, LcdBackend::WinUsb(_))) {
            warn!("Selected device is not a WinUSB LCD");
            return;
        }

        let family = self
            .registry
            .cached_usb_devices
            .iter()
            .find(|device| device.device_id == device_id)
            .map(|device| device.family);
        let (submit, receive) = std::sync::mpsc::sync_channel::<Option<ActiveTarget>>(1);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let identity = device_id.to_owned();
        let task = thread::Builder::new()
            .name("display-mode-switch".into())
            .spawn(move || {
                let target = receive.recv().context("Display switch handoff cancelled")?;
                anyhow::ensure!(
                    !worker_stop.load(Ordering::Acquire),
                    "Display mode switch cancelled"
                );
                if let Some(mut target) = target {
                    anyhow::ensure!(
                        matches!(target.lcd, LcdBackend::WinUsb(_)),
                        "Selected device is not a WinUSB LCD"
                    );
                    target.stop();
                    anyhow::ensure!(
                        !worker_stop.load(Ordering::Acquire),
                        "Display mode switch cancelled"
                    );
                    if let LcdBackend::WinUsb(lcd) = &mut target.lcd {
                        lcd.switch_to_desktop_mode()?;
                    }
                } else {
                    let family = family.context("Selected LCD is not in the device inventory")?;
                    let selected = lianli_devices::detect::enumerate_devices()?
                        .into_iter()
                        .find(|device| device.family == family && device.device_id() == identity)
                        .context("Selected LCD is no longer attached")?;
                    anyhow::ensure!(
                        !worker_stop.load(Ordering::Acquire),
                        "Display mode switch cancelled"
                    );
                    let mut lcd = WinUsbLcdDevice::open(selected.device, selected.pid)?;
                    anyhow::ensure!(
                        !worker_stop.load(Ordering::Acquire),
                        "Display mode switch cancelled"
                    );
                    lcd.switch_to_desktop_mode()?;
                }
                Ok(())
            });
        let task = match task {
            Ok(task) => task,
            Err(error) => {
                warn!("Could not start display mode switch: {error}");
                return;
            }
        };
        let target = target_idx.and_then(|idx| targets.remove(&idx));
        if let Err(error) = submit.send(target) {
            if let Some(target) = error.0 {
                targets.insert(target.index, target);
            }
            warn!("Display switch worker exited before accepting the target");
        }
        drop(targets);
        self.mark_mode_switch(device_id);
        self.display_switch = Some(DisplaySwitch {
            device_id: device_id.to_owned(),
            key: None,
            destination: "desktop",
            selected: None,
            task: Some(task),
            stop,
            deadline: Instant::now() + Duration::from_secs(10),
        });
    }

    pub(super) fn handle_display_switch_to_lcd(&mut self, device_id: &str, pid: u16) {
        if self.startup_image_job.is_some() {
            warn!("Startup image upload is still running");
            return;
        }
        if self.display_switch.is_some() {
            warn!("A display mode switch is already running");
            return;
        }
        let selected = lianli_devices::detect::enumerate_devices().and_then(|devices| {
            devices
                .into_iter()
                .find(|device| {
                    device.vid == lianli_devices::display_switcher::SWITCHER_VID
                        && device.pid == pid
                        && device.device_id() == device_id
                })
                .ok_or_else(|| anyhow::anyhow!("selected desktop display is no longer attached"))
        });
        let selected = match selected {
            Ok(selected) => selected,
            Err(error) => {
                warn!("Cannot switch {device_id} to LCD mode: {error:#}");
                return;
            }
        };
        let key = (selected.bus, selected.address);
        self.desktop_displays.stop_for_device(key);
        self.mark_mode_switch(device_id);
        self.display_switch = Some(DisplaySwitch {
            device_id: device_id.to_owned(),
            key: Some(key),
            destination: "LCD",
            selected: Some(selected),
            task: None,
            stop: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now() + Duration::from_secs(10),
        });
        self.poll_display_switch();
    }

    fn poll_display_switch(&mut self) {
        let Some(mut switch) = self.display_switch.take() else {
            return;
        };
        self.mark_mode_switch(&switch.device_id);
        if let Some(task) = &switch.task {
            if !task.is_finished() {
                self.display_switch = Some(switch);
                return;
            }
            match switch.task.take().unwrap().join() {
                Ok(Ok(())) => info!(
                    "Switched {} to {} mode",
                    switch.device_id, switch.destination
                ),
                Ok(Err(error)) => warn!(
                    "Failed to switch {} to {} mode: {error:#}",
                    switch.device_id, switch.destination
                ),
                Err(_) => warn!("Display mode switch worker panicked"),
            }
        } else if !self.desktop_displays.stop_for_device(switch.key.unwrap()) {
            if Instant::now() < switch.deadline {
                self.display_switch = Some(switch);
                return;
            }
            warn!(
                "Desktop worker did not stop in time to switch {}",
                switch.device_id
            );
        } else {
            let selected = switch.selected.take().unwrap();
            let backend = self.hid_backend();
            let stop = Arc::clone(&switch.stop);
            match thread::Builder::new()
                .name("display-mode-switch".into())
                .spawn(move || {
                    thread::sleep(Duration::from_millis(300));
                    anyhow::ensure!(
                        !stop.load(Ordering::Acquire),
                        "Display mode switch cancelled"
                    );
                    lianli_devices::display_switcher::switch_to_lcd_mode(&selected.device, backend)
                }) {
                Ok(task) => {
                    switch.task = Some(task);
                    self.display_switch = Some(switch);
                    return;
                }
                Err(error) => warn!("Could not start display mode switch: {error}"),
            }
        }
        if let Some(key) = switch.key {
            self.desktop_displays.finish_switch(key);
        }
        self.schedule_post_switch_refresh();
    }

    fn schedule_post_switch_refresh(&mut self) {
        self.post_switch_refresh.schedule(Instant::now());
    }

    pub(super) fn refresh_after_mode_switch(&mut self) {
        self.poll_display_switch();
        let now = Instant::now();
        self.mode_switch_suppression.retain(|_, until| now < *until);
        if self.post_switch_refresh.take_due(now) {
            self.refresh_usb_device_cache();
            if self.post_switch_refresh.is_idle() {
                report_stale_usb_enumeration();
            }
        }
    }

    fn mark_mode_switch(&mut self, device_id: &str) {
        self.mode_switch_suppression.insert(
            device_id.to_string(),
            Instant::now() + Duration::from_secs(8),
        );
    }

    pub(super) fn mode_switch_suppressed(&self, device_id: &str) -> bool {
        self.display_switch
            .as_ref()
            .is_some_and(|switch| switch.device_id == device_id)
            || self
                .mode_switch_suppression
                .get(device_id)
                .is_some_and(|until| Instant::now() < *until)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_desktop_switch_preserves_suppression_until_worker_completion() {
        let mut service = ServiceManager::new(
            "unused-config.json".into(),
            "unused-socket".into(),
            lianli_shared::daemon::DaemonMode::User,
        )
        .unwrap();
        let (release, wait) = std::sync::mpsc::channel();
        service.display_switch = Some(DisplaySwitch {
            device_id: "fixture".into(),
            key: None,
            destination: "desktop",
            selected: None,
            task: Some(thread::spawn(move || {
                wait.recv_timeout(Duration::from_secs(2))?;
                anyhow::bail!("fixture command failed")
            })),
            stop: Arc::new(AtomicBool::new(false)),
            deadline: Instant::now(),
        });
        service
            .mode_switch_suppression
            .insert("fixture".into(), Instant::now());
        assert!(service.mode_switch_suppressed("fixture"));
        assert!(!service.mode_switch_suppressed("unrelated"));
        let started = Instant::now();
        service.poll_display_switch();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert!(service.display_switch.is_some());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !service
            .display_switch
            .as_ref()
            .unwrap()
            .task
            .as_ref()
            .unwrap()
            .is_finished()
        {
            assert!(Instant::now() < deadline);
            thread::sleep(Duration::from_millis(1));
        }
        service.poll_display_switch();
        assert!(service.display_switch.is_none());
        assert!(service.mode_switch_suppressed("fixture"));
        assert!(service
            .post_switch_refresh
            .take_due(Instant::now() + REFRESH_INTERVAL));
    }

    #[test]
    fn switch_shutdown_cancels_and_joins_its_worker() {
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let (finished, completion) = std::sync::mpsc::channel();
        let task = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            while !worker_stop.load(Ordering::Acquire) && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(1));
            }
            finished.send(worker_stop.load(Ordering::Acquire)).unwrap();
            Ok(())
        });
        drop(DisplaySwitch {
            device_id: "fixture".into(),
            key: Some((1, 2)),
            destination: "LCD",
            selected: None,
            task: Some(task),
            stop,
            deadline: Instant::now(),
        });
        assert!(completion.try_recv().unwrap());
    }

    #[test]
    fn switches_coalesce_without_postponing_the_next_refresh() {
        let now = Instant::now();
        let mut refresh = PostSwitchRefresh::default();
        assert!(!refresh.take_due(now));
        refresh.schedule(now);
        refresh.schedule(now + Duration::from_secs(2));
        assert!(!refresh.take_due(now + Duration::from_secs(2)));
        for seconds in [3, 6, 9, 11] {
            assert!(refresh.take_due(now + Duration::from_secs(seconds)));
            assert!(!refresh.take_due(now + Duration::from_secs(seconds)));
        }
        assert!(!refresh.take_due(now + Duration::from_secs(30)));
    }

    #[test]
    fn delayed_poll_refreshes_once_and_a_later_switch_starts_a_new_window() {
        let now = Instant::now();
        let mut refresh = PostSwitchRefresh::default();
        refresh.schedule(now);
        assert!(refresh.take_due(now + Duration::from_secs(30)));
        assert!(!refresh.take_due(now + Duration::from_secs(30)));
        refresh.schedule(now + Duration::from_secs(31));
        for seconds in [34, 37, 40] {
            assert!(refresh.take_due(now + Duration::from_secs(seconds)));
        }
        assert!(!refresh.take_due(now + Duration::from_secs(43)));
    }
}
