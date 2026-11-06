//! Power and device sounds from kernel uevents.
//!
//! One thread holds a `NETLINK_KOBJECT_UEVENT` socket and sleeps in `poll`
//! until the kernel reports something: there is no timer while nothing
//! happens. On an event it reads the few sysfs files that matter:
//!
//! - `power_supply`: the charger (`Mains`/`USB` with `online`) and the
//!   batteries (`capacity`, `status`), for plug/unplug and the battery levels.
//! - `usb` devices, `sound` cards, `drm` connectors (monitor hotplug) and,
//!   when enabled, `bluetooth` links, for connect/disconnect.
//!
//! A dock sends dozens of uevents at once. Changes are gathered for a short
//! moment and played as one sound; a charger in the same burst wins over the
//! devices. After a sound, more changes in the same direction stay quiet for
//! `debounce_ms`. Right after a resume the kernel re-reports devices, so
//! device sounds pause for a few seconds then.

use crate::sounds::{Cached, Paths, Request, Settings};
use std::{
    collections::BTreeSet,
    io::Read,
    os::unix::io::{AsRawFd, RawFd},
    path::{Path, PathBuf},
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

/// How long a burst gathers after its last change, and at most in total.
const GATHER: Duration = Duration::from_millis(250);
const GATHER_MAX: Duration = Duration::from_millis(1200);
/// Device sounds pause this long after a resume.
const RESUME_QUIET: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Status {
    Charging,
    Discharging,
    Full,
    NotCharging,
    #[default]
    Unknown,
}

impl Status {
    fn parse(text: &str) -> Self {
        match text.trim() {
            "Charging" => Status::Charging,
            "Discharging" => Status::Discharging,
            "Full" => Status::Full,
            "Not charging" => Status::NotCharging,
            _ => Status::Unknown,
        }
    }
}

/// The charger and the combined battery.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Power {
    pub ac: Option<bool>,
    pub capacity: Option<u8>,
    pub status: Status,
}

impl Power {
    fn discharging(&self) -> bool {
        self.capacity.is_some() && (self.status == Status::Discharging || self.ac == Some(false))
    }
}

/// sysfs, or a directory that looks like it in tests.
#[derive(Clone, Debug)]
pub struct Sysfs {
    pub root: PathBuf,
}

fn read(path: &Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

impl Sysfs {
    pub fn from_env() -> Self {
        Self {
            root: std::env::var_os("NIRUNOTIFY_SYSFS")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("/sys")),
        }
    }

    /// Supplies with `scope = Device` (a mouse battery, a phone) are not the
    /// machine's own and are ignored, as are batteries without a capacity.
    pub fn power(&self) -> Power {
        let mut power = Power::default();
        let Ok(entries) = std::fs::read_dir(self.root.join("class/power_supply")) else {
            return power;
        };
        let mut energy = (0u64, 0u64);
        let mut capacities = Vec::new();
        let mut statuses = Vec::new();
        let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        names.sort();
        for dir in names {
            if read(&dir.join("scope")).as_deref() == Some("Device") {
                continue;
            }
            match read(&dir.join("type")).as_deref() {
                Some("Mains") | Some("USB") => {
                    if let Some(online) = read(&dir.join("online")) {
                        let on = online == "1";
                        power.ac = Some(power.ac.unwrap_or(false) || on);
                    }
                }
                Some("Battery") => {
                    let Some(capacity) =
                        read(&dir.join("capacity")).and_then(|c| c.parse::<u8>().ok())
                    else {
                        continue;
                    };
                    capacities.push(capacity.min(100));
                    statuses.push(Status::parse(
                        &read(&dir.join("status")).unwrap_or_default(),
                    ));
                    let now =
                        read(&dir.join("energy_now")).or_else(|| read(&dir.join("charge_now")));
                    let full =
                        read(&dir.join("energy_full")).or_else(|| read(&dir.join("charge_full")));
                    if let (Some(n), Some(f)) = (
                        now.and_then(|v| v.parse::<u64>().ok()),
                        full.and_then(|v| v.parse::<u64>().ok()),
                    ) {
                        energy.0 += n;
                        energy.1 += f;
                    }
                }
                _ => {}
            }
        }
        if capacities.is_empty() {
            return power;
        }
        power.capacity = Some(if capacities.len() > 1 && energy.1 > 0 {
            ((energy.0 * 100 + energy.1 / 2) / energy.1).min(100) as u8
        } else {
            (capacities.iter().map(|&c| u32::from(c)).sum::<u32>() / capacities.len() as u32) as u8
        });
        power.status = if statuses.contains(&Status::Discharging) {
            Status::Discharging
        } else if statuses.contains(&Status::Charging) {
            Status::Charging
        } else if statuses.iter().all(|s| *s == Status::Full) {
            Status::Full
        } else if statuses.contains(&Status::NotCharging) {
            Status::NotCharging
        } else {
            Status::Unknown
        };
        power
    }

    /// Connected drm connectors (card0-HDMI-A-1, …).
    pub fn connectors(&self) -> BTreeSet<String> {
        let Ok(entries) = std::fs::read_dir(self.root.join("class/drm")) else {
            return BTreeSet::new();
        };
        entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                (name.starts_with("card") && name.contains('-')).then_some(name)
            })
            .filter(|name| {
                read(&self.root.join("class/drm").join(name).join("status")).as_deref()
                    == Some("connected")
            })
            .collect()
    }
}

/// One line for `nirunotify --check-config`: what the watcher reads now.
pub fn describe() -> String {
    let sysfs = Sysfs::from_env();
    let power = sysfs.power();
    let ac = match power.ac {
        Some(true) => "on",
        Some(false) => "off",
        None => "none",
    };
    let battery = match power.capacity {
        Some(c) => format!("{c}% {:?}", power.status),
        None => "none".into(),
    };
    let displays: Vec<String> = sysfs.connectors().into_iter().collect();
    let netlink = open_netlink()
        .map(|fd| {
            unsafe { libc::close(fd) };
            "ok"
        })
        .unwrap_or("unavailable");
    format!(
        "sounds: charger={ac} battery={battery} displays=[{}] uevents={netlink}",
        displays.join(", ")
    )
}

/// Battery events, once per discharge (or charge).
#[derive(Debug)]
pub struct PowerTracker {
    last: Power,
    low_done: bool,
    critical_done: bool,
    full_done: bool,
}

impl PowerTracker {
    /// The state at start is taken silently: nothing plays for a condition
    /// that was already true before NIRUNOTIFY ran.
    pub fn new(initial: Power, s: &Settings) -> Self {
        let discharging = initial.discharging();
        let cap = initial.capacity.unwrap_or(100);
        Self {
            last: initial,
            low_done: discharging && cap <= s.battery_low(),
            critical_done: discharging && cap <= s.battery_critical(),
            full_done: !discharging && Self::charged(&initial, initial.status, s),
        }
    }

    /// Charging has finished: full, or stopped by a charge limit on AC.
    fn charged(now: &Power, before: Status, s: &Settings) -> bool {
        let cap = now.capacity.unwrap_or(0);
        now.status == Status::Full
            || (now.status == Status::Charging && cap >= s.battery_full())
            || (now.status == Status::NotCharging
                && matches!(before, Status::Charging | Status::NotCharging)
                && now.ac != Some(false))
    }

    /// The events for a new reading: charger first, then the battery.
    pub fn update(&mut self, now: Power, s: &Settings) -> Vec<&'static str> {
        let mut events = Vec::new();
        if let (Some(before), Some(after)) = (self.last.ac, now.ac) {
            if before != after {
                events.push(if after { "power-plug" } else { "power-unplug" });
            }
        }
        if let Some(cap) = now.capacity {
            if now.discharging() {
                self.full_done = false;
                if cap <= s.battery_critical() && !self.critical_done {
                    self.critical_done = true;
                    self.low_done = true;
                    events.push("battery-critical");
                } else if cap <= s.battery_low() && !self.low_done {
                    self.low_done = true;
                    events.push("battery-low");
                }
            } else {
                // A new discharge may warn again.
                self.low_done = false;
                self.critical_done = false;
                if Self::charged(&now, self.last.status, s) {
                    if !self.full_done {
                        self.full_done = true;
                        // Only a charge that was seen happening is announced.
                        if self.last.status == Status::Charging {
                            events.push("battery-full");
                        }
                    }
                } else if now.status == Status::Charging && cap < s.battery_full() {
                    self.full_done = false;
                }
            }
        }
        self.last = now;
        events
    }
}

/// One change for the burst: the charger or a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Change {
    Power(&'static str),
    Device { connect: bool },
}

/// Gathers changes and decides the one sound for them.
#[derive(Debug, Default)]
pub struct Burst {
    started: Option<Instant>,
    last: Option<Instant>,
    power: Option<&'static str>,
    connects: u32,
    disconnects: u32,
    last_connect: bool,
    /// After a sound: more devices in this direction stay quiet until then.
    quiet: Option<(Instant, bool)>,
}

impl Burst {
    pub fn push(&mut self, change: Change, now: Instant, debounce: Duration) {
        if let Change::Device { connect } = change {
            if let Some((until, direction)) = self.quiet {
                if now < until && direction == connect && self.started.is_none() {
                    // The same dock still settling: stay quiet a little longer.
                    self.quiet = Some(((now + debounce).max(until), direction));
                    return;
                }
            }
        }
        self.started.get_or_insert(now);
        self.last = Some(now);
        match change {
            Change::Power(event) => self.power = Some(event),
            Change::Device { connect } => {
                if connect {
                    self.connects += 1;
                } else {
                    self.disconnects += 1;
                }
                self.last_connect = connect;
            }
        }
    }

    /// When the gathered changes should sound.
    pub fn deadline(&self) -> Option<Instant> {
        let started = self.started?;
        let last = self.last.unwrap_or(started);
        Some((last + GATHER).min(started + GATHER_MAX))
    }

    /// The sound for the gathered changes, once their deadline has passed.
    pub fn flush(&mut self, now: Instant, debounce: Duration) -> Option<&'static str> {
        if self.deadline().is_none_or(|d| now < d) {
            return None;
        }
        let power = self.power.take();
        let (connects, disconnects) = (self.connects, self.disconnects);
        let last_connect = self.last_connect;
        self.started = None;
        self.last = None;
        self.connects = 0;
        self.disconnects = 0;
        if let Some(event) = power {
            // Devices that come with the charger (a dock) are part of it.
            self.quiet = Some((now + debounce, event == "power-plug"));
            return Some(event);
        }
        if connects + disconnects == 0 {
            return None;
        }
        let connect = match (connects, disconnects) {
            (_, 0) => true,
            (0, _) => false,
            _ => last_connect,
        };
        if let Some((until, direction)) = self.quiet {
            if now < until && direction == connect {
                self.quiet = Some((now + debounce, direction));
                return None;
            }
        }
        self.quiet = Some((now + debounce, connect));
        Some(if connect {
            "device-connect"
        } else {
            "device-disconnect"
        })
    }
}

/// The fields of one kernel uevent that matter here.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Uevent {
    pub action: String,
    pub subsystem: String,
    pub devtype: String,
    pub devpath: String,
}

impl Uevent {
    /// `action@devpath\0KEY=value\0…`; udev's own messages are ignored.
    pub fn parse(message: &[u8]) -> Option<Self> {
        if message.starts_with(b"libudev") {
            return None;
        }
        let mut event = Uevent::default();
        for field in message.split(|b| *b == 0) {
            let text = String::from_utf8_lossy(field);
            if let Some((key, value)) = text.split_once('=') {
                match key {
                    "ACTION" => event.action = value.to_string(),
                    "SUBSYSTEM" => event.subsystem = value.to_string(),
                    "DEVTYPE" => event.devtype = value.to_string(),
                    "DEVPATH" => event.devpath = value.to_string(),
                    _ => {}
                }
            }
        }
        (!event.action.is_empty() && !event.subsystem.is_empty()).then_some(event)
    }

    fn leaf(&self) -> &str {
        self.devpath.rsplit('/').next().unwrap_or("")
    }

    /// A device that appeared or went away, by source; None otherwise.
    fn device(&self, s: &Settings) -> Option<bool> {
        let connect = match self.action.as_str() {
            "add" => true,
            "remove" => false,
            _ => return None,
        };
        let source = match self.subsystem.as_str() {
            // Root hubs come and go with controllers, not with plugs.
            "usb" if self.devtype == "usb_device" && !self.leaf().starts_with("usb") => "usb",
            "sound"
                if self
                    .leaf()
                    .strip_prefix("card")
                    .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())) =>
            {
                "audio"
            }
            "bluetooth" if self.devtype == "link" => "bluetooth",
            _ => return None,
        };
        s.device_source(source).then_some(connect)
    }
}

/// Seconds the machine has spent suspended since boot.
fn suspended_seconds() -> f64 {
    let read = |clock| {
        let mut ts: libc::timespec = unsafe { std::mem::zeroed() };
        unsafe { libc::clock_gettime(clock, &mut ts) };
        ts.tv_sec as f64 + ts.tv_nsec as f64 / 1e9
    };
    read(libc::CLOCK_BOOTTIME) - read(libc::CLOCK_MONOTONIC)
}

/// Everything the watcher knows between two events.
pub struct Watcher {
    pub sysfs: Sysfs,
    pub settings: Cached,
    pub power: PowerTracker,
    pub connectors: BTreeSet<String>,
    pub burst: Burst,
    suspended: f64,
    resume_quiet_until: Option<Instant>,
}

impl Watcher {
    pub fn new(sysfs: Sysfs, mut settings: Cached) -> Self {
        let s = settings.refresh().clone();
        let power = PowerTracker::new(sysfs.power(), &s);
        let connectors = sysfs.connectors();
        Self {
            sysfs,
            settings,
            power,
            connectors,
            burst: Burst::default(),
            suspended: suspended_seconds(),
            resume_quiet_until: None,
        }
    }

    /// Handle one uevent; immediate sounds (battery levels) are returned,
    /// charger and device changes go into the burst.
    pub fn handle(&mut self, event: &Uevent, now: Instant) -> Vec<&'static str> {
        let s = self.settings.refresh().clone();
        let suspended = suspended_seconds();
        if suspended - self.suspended > 1.0 {
            self.resume_quiet_until = Some(now + RESUME_QUIET);
        }
        self.suspended = suspended;
        let resuming = self.resume_quiet_until.is_some_and(|until| now < until);
        let mut immediate = Vec::new();
        match event.subsystem.as_str() {
            "power_supply" => {
                for e in self.power.update(self.sysfs.power(), &s) {
                    match e {
                        "power-plug" | "power-unplug" => {
                            self.burst.push(Change::Power(e), now, s.debounce())
                        }
                        other => immediate.push(other),
                    }
                }
            }
            "drm" if event.action == "change" => {
                let now_connected = self.sysfs.connectors();
                let added = now_connected.difference(&self.connectors).count();
                let removed = self.connectors.difference(&now_connected).count();
                self.connectors = now_connected;
                if s.device_source("displays") && !resuming {
                    if added > 0 {
                        self.burst
                            .push(Change::Device { connect: true }, now, s.debounce());
                    }
                    if removed > 0 {
                        self.burst
                            .push(Change::Device { connect: false }, now, s.debounce());
                    }
                }
            }
            _ => {
                if let Some(connect) = event.device(&s) {
                    if !resuming {
                        self.burst
                            .push(Change::Device { connect }, now, s.debounce());
                    }
                }
            }
        }
        immediate
    }

    pub fn flush(&mut self, now: Instant) -> Option<&'static str> {
        let debounce = self.settings.refresh().debounce();
        self.burst.flush(now, debounce)
    }
}

fn open_netlink() -> Option<RawFd> {
    const NETLINK_KOBJECT_UEVENT: libc::c_int = 15;
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_DGRAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = 1; // kernel broadcasts
    let bound = unsafe {
        libc::bind(
            fd,
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if bound != 0 {
        unsafe { libc::close(fd) };
        return None;
    }
    Some(fd)
}

/// Turn a test line (`ACTION=add SUBSYSTEM=usb …`) into a uevent message.
pub fn message_from_line(line: &str) -> Vec<u8> {
    let mut out = b"test@/\0".to_vec();
    for word in line.split_whitespace() {
        out.extend_from_slice(word.as_bytes());
        out.push(0);
    }
    out
}

/// Run the watcher. TEST, when given, is a pipe of injected test uevents
/// (one per line), used only when NIRUNOTIFY_TEST_UEVENTS=1.
pub fn start(sounds: Sender<Request>, test: Option<std::fs::File>) {
    let _ = std::thread::Builder::new()
        .name("nirunotify-uevents".into())
        .spawn(move || run(sounds, test));
}

fn run(sounds: Sender<Request>, mut test: Option<std::fs::File>) {
    let netlink = if std::env::var_os("NIRUNOTIFY_SYSFS").is_some() {
        None // a test tree must not see the real machine's events
    } else {
        open_netlink()
    };
    if netlink.is_none() && test.is_none() {
        return;
    }
    let mut watcher = Watcher::new(Sysfs::from_env(), Cached::new(Paths::from_env().config));
    let mut buffer = vec![0u8; 16384];
    let mut pending = String::new();
    loop {
        let timeout = watcher
            .burst
            .deadline()
            .map(|d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .min(10_000) as libc::c_int
            })
            .unwrap_or(-1);
        let mut fds = Vec::new();
        if let Some(fd) = netlink {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }
        if let Some(file) = &test {
            fds.push(libc::pollfd {
                fd: file.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            });
        }
        let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        let mut messages: Vec<Vec<u8>> = Vec::new();
        if let Some(fd) = netlink {
            loop {
                let read = unsafe {
                    libc::recv(
                        fd,
                        buffer.as_mut_ptr() as *mut libc::c_void,
                        buffer.len(),
                        0,
                    )
                };
                if read > 0 {
                    messages.push(buffer[..read as usize].to_vec());
                    continue;
                }
                if read < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS)
                {
                    // Events were lost: take the state again, silently.
                    let s = watcher.settings.refresh().clone();
                    watcher.power = PowerTracker::new(watcher.sysfs.power(), &s);
                    watcher.connectors = watcher.sysfs.connectors();
                    continue;
                }
                break;
            }
        }
        if let Some(file) = test.as_mut() {
            let readable = fds
                .last()
                .is_some_and(|p| p.revents & (libc::POLLIN | libc::POLLHUP) != 0);
            if readable {
                let mut chunk = [0u8; 4096];
                match file.read(&mut chunk) {
                    Ok(0) | Err(_) => test = None,
                    Ok(n) => pending.push_str(&String::from_utf8_lossy(&chunk[..n])),
                }
                while let Some(end) = pending.find('\n') {
                    let line: String = pending.drain(..=end).collect();
                    messages.push(message_from_line(&line));
                }
            }
        }
        let now = Instant::now();
        for message in messages {
            if let Some(event) = Uevent::parse(&message) {
                crate::sounds::debug(&format!(
                    "uevent {} {} {} {}",
                    event.action, event.subsystem, event.devtype, event.devpath
                ));
                for e in watcher.handle(&event, now) {
                    let _ = sounds.send(Request::new(e));
                }
            }
        }
        if let Some(e) = watcher.flush(Instant::now()) {
            let _ = sounds.send(Request::new(e));
        }
        if netlink.is_none() && test.is_none() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> Settings {
        Settings::parse(include_str!("../tests/fixtures/sounds-default.toml"))
    }

    fn power(ac: bool, cap: u8, status: Status) -> Power {
        Power {
            ac: Some(ac),
            capacity: Some(cap),
            status,
        }
    }

    #[test]
    fn plug_and_unplug_follow_the_charger() {
        let s = defaults();
        let mut t = PowerTracker::new(power(true, 80, Status::Charging), &s);
        assert_eq!(
            t.update(power(true, 81, Status::Charging), &s),
            Vec::<&str>::new()
        );
        assert_eq!(
            t.update(power(false, 81, Status::Discharging), &s),
            vec!["power-unplug"]
        );
        assert_eq!(
            t.update(power(true, 81, Status::Charging), &s),
            vec!["power-plug"]
        );
    }

    #[test]
    fn battery_low_and_critical_sound_once_per_discharge() {
        let s = defaults();
        let mut t = PowerTracker::new(power(false, 30, Status::Discharging), &s);
        assert!(t
            .update(power(false, 21, Status::Discharging), &s)
            .is_empty());
        assert_eq!(
            t.update(power(false, 20, Status::Discharging), &s),
            vec!["battery-low"]
        );
        assert!(t
            .update(power(false, 19, Status::Discharging), &s)
            .is_empty());
        assert!(t
            .update(power(false, 12, Status::Discharging), &s)
            .is_empty());
        assert_eq!(
            t.update(power(false, 5, Status::Discharging), &s),
            vec!["battery-critical"]
        );
        assert!(t
            .update(power(false, 4, Status::Discharging), &s)
            .is_empty());
        // Charging re-arms; the next discharge warns again.
        assert_eq!(
            t.update(power(true, 6, Status::Charging), &s),
            vec!["power-plug"]
        );
        assert_eq!(
            t.update(power(false, 6, Status::Discharging), &s),
            vec!["power-unplug", "battery-low"]
        );
    }

    #[test]
    fn a_jump_past_both_thresholds_plays_only_critical() {
        let s = defaults();
        let mut t = PowerTracker::new(power(false, 40, Status::Discharging), &s);
        assert_eq!(
            t.update(power(false, 3, Status::Discharging), &s),
            vec!["battery-critical"]
        );
        assert!(t
            .update(power(false, 2, Status::Discharging), &s)
            .is_empty());
    }

    #[test]
    fn a_low_battery_at_start_is_not_announced() {
        let s = defaults();
        let mut t = PowerTracker::new(power(false, 15, Status::Discharging), &s);
        assert!(t
            .update(power(false, 14, Status::Discharging), &s)
            .is_empty());
        assert_eq!(
            t.update(power(false, 5, Status::Discharging), &s),
            vec!["battery-critical"]
        );
    }

    #[test]
    fn thresholds_come_from_the_config() {
        let s = Settings::parse("[sounds.power]\nbattery_low_percent = 40\nbattery_critical_percent = 10\nbattery_full_percent = 80\n");
        let mut t = PowerTracker::new(power(false, 50, Status::Discharging), &s);
        assert_eq!(
            t.update(power(false, 40, Status::Discharging), &s),
            vec!["battery-low"]
        );
        assert_eq!(
            t.update(power(false, 10, Status::Discharging), &s),
            vec!["battery-critical"]
        );
        assert_eq!(
            t.update(power(true, 70, Status::Charging), &s),
            vec!["power-plug"]
        );
        assert_eq!(
            t.update(power(true, 80, Status::Charging), &s),
            vec!["battery-full"]
        );
        assert!(t.update(power(true, 81, Status::Charging), &s).is_empty());
    }

    #[test]
    fn full_is_announced_for_a_seen_charge_including_a_charge_limit() {
        let s = defaults();
        let mut t = PowerTracker::new(power(true, 98, Status::NotCharging), &s);
        assert!(t
            .update(power(true, 98, Status::NotCharging), &s)
            .is_empty());
        let mut t = PowerTracker::new(power(true, 95, Status::Charging), &s);
        assert_eq!(
            t.update(power(true, 98, Status::NotCharging), &s),
            vec!["battery-full"]
        );
        let mut t = PowerTracker::new(power(true, 99, Status::Charging), &s);
        assert_eq!(
            t.update(power(true, 100, Status::Full), &s),
            vec!["battery-full"]
        );
        assert!(t.update(power(true, 100, Status::Full), &s).is_empty());
    }

    #[test]
    fn a_desktop_without_battery_is_silent() {
        let s = defaults();
        let none = Power::default();
        let mut t = PowerTracker::new(none, &s);
        assert!(t.update(none, &s).is_empty());
    }

    #[test]
    fn a_dock_burst_is_one_sound_and_the_charger_wins() {
        let d = Duration::from_millis(1500);
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut b = Burst::default();
        for (i, at) in [0u64, 40, 90, 150, 300].iter().enumerate() {
            b.push(Change::Device { connect: true }, ms(*at), d);
            if i == 2 {
                b.push(Change::Power("power-plug"), ms(*at), d);
            }
        }
        assert_eq!(b.flush(ms(400), d), None, "still gathering");
        assert_eq!(b.flush(ms(560), d), Some("power-plug"));
        // Stragglers from the same dock stay quiet.
        b.push(Change::Device { connect: true }, ms(1200), d);
        assert_eq!(b.flush(ms(2000), d), None);
        // Unplugging the dock is heard.
        b.push(Change::Device { connect: false }, ms(2100), d);
        b.push(Change::Power("power-unplug"), ms(2150), d);
        assert_eq!(b.flush(ms(2500), d), Some("power-unplug"));
    }

    #[test]
    fn devices_coalesce_and_debounce() {
        let d = Duration::from_millis(1500);
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let mut b = Burst::default();
        b.push(Change::Device { connect: true }, ms(0), d);
        b.push(Change::Device { connect: true }, ms(100), d);
        assert_eq!(b.deadline(), Some(ms(350)));
        assert_eq!(b.flush(ms(350), d), Some("device-connect"));
        assert_eq!(b.deadline(), None);
        // A second USB stick 0.5 s later is part of the same burst.
        b.push(Change::Device { connect: true }, ms(850), d);
        assert_eq!(b.flush(ms(1200), d), None);
        // A new plug after the debounce sounds again.
        b.push(Change::Device { connect: true }, ms(4000), d);
        assert_eq!(b.flush(ms(4300), d), Some("device-connect"));
        // Pulling it right away is a different direction and is heard.
        b.push(Change::Device { connect: false }, ms(4400), d);
        assert_eq!(b.flush(ms(4700), d), Some("device-disconnect"));
        // A burst cannot gather forever.
        let mut b = Burst::default();
        for n in 0..20 {
            b.push(Change::Device { connect: true }, ms(n * 200), d);
        }
        assert_eq!(b.deadline(), Some(ms(1200)));
    }

    #[test]
    fn uevents_parse_and_classify() {
        let s = defaults();
        let usb = b"add@/devices/pci0000:00/0000:00:14.0/usb1/1-2\0ACTION=add\0DEVPATH=/devices/pci0000:00/0000:00:14.0/usb1/1-2\0SUBSYSTEM=usb\0DEVTYPE=usb_device\0";
        let e = Uevent::parse(usb).unwrap();
        assert_eq!(e.device(&s), Some(true));
        let iface = Uevent::parse(&message_from_line(
            "ACTION=add SUBSYSTEM=usb DEVTYPE=usb_interface DEVPATH=/d/usb1/1-2/1-2:1.0",
        ))
        .unwrap();
        assert_eq!(
            iface.device(&s),
            None,
            "interfaces are part of their device"
        );
        let hub = Uevent::parse(&message_from_line(
            "ACTION=add SUBSYSTEM=usb DEVTYPE=usb_device DEVPATH=/d/usb3",
        ))
        .unwrap();
        assert_eq!(hub.device(&s), None, "root hubs are not plugs");
        let card = Uevent::parse(&message_from_line(
            "ACTION=remove SUBSYSTEM=sound DEVPATH=/d/1-2:1.0/sound/card2",
        ))
        .unwrap();
        assert_eq!(card.device(&s), Some(false));
        let pcm = Uevent::parse(&message_from_line(
            "ACTION=remove SUBSYSTEM=sound DEVPATH=/d/sound/card2/pcmC2D0p",
        ))
        .unwrap();
        assert_eq!(pcm.device(&s), None);
        let bt = Uevent::parse(&message_from_line(
            "ACTION=add SUBSYSTEM=bluetooth DEVTYPE=link DEVPATH=/d/hci0/hci0:3585",
        ))
        .unwrap();
        assert_eq!(bt.device(&s), None, "bluetooth is off by default");
        let on = Settings::parse("[sounds.devices]\nbluetooth = true\nusb = false\n");
        assert_eq!(bt.device(&on), Some(true));
        assert_eq!(e.device(&on), None);
        assert_eq!(Uevent::parse(b"libudev\0\xfe\xed"), None);
    }

    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("nirunotify-sysfs-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        root
    }

    fn put(root: &Path, file: &str, value: &str) {
        let path = root.join(file);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("{value}\n")).unwrap();
    }

    #[test]
    fn sysfs_power_and_connectors() {
        let root = tree("power");
        put(&root, "class/power_supply/AC/type", "Mains");
        put(&root, "class/power_supply/AC/online", "0");
        put(
            &root,
            "class/power_supply/ucsi-source-psy-USBC000:001/type",
            "USB",
        );
        put(
            &root,
            "class/power_supply/ucsi-source-psy-USBC000:001/online",
            "1",
        );
        put(&root, "class/power_supply/BAT0/type", "Battery");
        put(&root, "class/power_supply/BAT0/capacity", "40");
        put(&root, "class/power_supply/BAT0/status", "Charging");
        put(&root, "class/power_supply/hidpp_battery_0/type", "Battery");
        put(&root, "class/power_supply/hidpp_battery_0/scope", "Device");
        put(&root, "class/power_supply/hidpp_battery_0/capacity", "5");
        put(&root, "class/drm/card1-HDMI-A-1/status", "connected");
        put(&root, "class/drm/card1-DP-1/status", "disconnected");
        put(&root, "class/drm/card1/dev", "226:1");
        let sys = Sysfs { root: root.clone() };
        assert_eq!(sys.power(), power(true, 40, Status::Charging));
        assert_eq!(
            sys.connectors().into_iter().collect::<Vec<_>>(),
            vec!["card1-HDMI-A-1"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_watcher_turns_uevents_into_sounds() {
        let root = tree("watcher");
        put(&root, "class/power_supply/AC/type", "Mains");
        put(&root, "class/power_supply/AC/online", "1");
        put(&root, "class/power_supply/BAT0/type", "Battery");
        put(&root, "class/power_supply/BAT0/capacity", "22");
        put(&root, "class/power_supply/BAT0/status", "Charging");
        put(&root, "class/drm/card0-DP-1/status", "disconnected");
        let cfg = root.join("sounds.toml");
        std::fs::write(&cfg, include_str!("../tests/fixtures/sounds-default.toml")).unwrap();
        let mut w = Watcher::new(Sysfs { root: root.clone() }, Cached::new(cfg));
        let t = Instant::now();
        let ms = |n| t + Duration::from_millis(n);
        let supply = Uevent::parse(&message_from_line(
            "ACTION=change SUBSYSTEM=power_supply DEVPATH=/d/AC",
        ))
        .unwrap();
        put(&root, "class/power_supply/AC/online", "0");
        put(&root, "class/power_supply/BAT0/status", "Discharging");
        put(&root, "class/power_supply/BAT0/capacity", "20");
        assert_eq!(w.handle(&supply, ms(0)), vec!["battery-low"]);
        assert_eq!(w.flush(ms(300)), Some("power-unplug"));
        put(&root, "class/drm/card0-DP-1/status", "connected");
        let drm = Uevent::parse(&message_from_line(
            "ACTION=change SUBSYSTEM=drm DEVPATH=/d/drm/card0",
        ))
        .unwrap();
        assert!(w.handle(&drm, ms(5000)).is_empty());
        assert_eq!(w.flush(ms(5300)), Some("device-connect"));
        // A repeated hotplug with nothing new is silent.
        assert!(w.handle(&drm, ms(9000)).is_empty());
        assert_eq!(w.flush(ms(9300)), None);
        let _ = std::fs::remove_dir_all(root);
    }
}
