//! Network sounds: the connection is lost (`network-down`) and back
//! (`network-up`).
//!
//! One thread holds a `NETLINK_ROUTE` socket subscribed to link and IPv4/IPv6
//! route changes and sleeps in `poll` until the kernel reports one: there is
//! no timer and no polling while nothing changes, and no NetworkManager or
//! D-Bus dependency (dhcpcd, iwd and static setups work the same).
//!
//! "Online" means a default route (IPv4 or IPv6, not a reject route, not on
//! `lo`) through an interface whose `operstate` is `up` or `unknown` (VPN and
//! point-to-point links report `unknown`). The tables are read from
//! `/proc/net/route` and `/proc/net/ipv6_route` only when the kernel reports a
//! change.
//!
//! The sounds are deliberately reluctant:
//!
//! - `network-down` needs the connection to have been up since the daemon
//!   started, to stay gone for `DOWN_AFTER`, and never sounds in the first
//!   `START_QUIET` of a session or within `RESUME_QUIET` after a resume (Wi-Fi
//!   reconnects then). A machine that starts offline is never announced.
//! - `network-up` sounds only after a `network-down`, once the connection has
//!   been back for `UP_AFTER`.
//! - After a `network-up`, the next `network-down` waits at least
//!   `FLAP_QUIET`, so a flapping link is one pair of sounds, not a stream.

use crate::sounds::{debug, Request};
use std::{
    os::unix::io::RawFd,
    path::PathBuf,
    sync::mpsc::Sender,
    time::{Duration, Instant},
};

/// How long the connection must be gone before `network-down`.
pub const DOWN_AFTER: Duration = Duration::from_secs(6);
/// How long it must be back before `network-up`.
pub const UP_AFTER: Duration = Duration::from_secs(3);
/// No `network-down` this long after the daemon starts (the session start).
pub const START_QUIET: Duration = Duration::from_secs(30);
/// No `network-down` this long after a resume.
pub const RESUME_QUIET: Duration = Duration::from_secs(20);
/// At least this long between a `network-up` and the next `network-down`.
pub const FLAP_QUIET: Duration = Duration::from_secs(30);

const RTF_UP: u32 = 0x0001;
const RTF_REJECT: u32 = 0x0200;

/// The interface of the first usable IPv4 default route in `/proc/net/route`.
pub fn default_route_v4(table: &str, up: &dyn Fn(&str) -> bool) -> Option<String> {
    table.lines().skip(1).find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 8 || f[0] == "lo" || f[1] != "00000000" || f[7] != "00000000" {
            return None;
        }
        let flags = u32::from_str_radix(f[3], 16).ok()?;
        (flags & RTF_UP != 0 && flags & RTF_REJECT == 0 && up(f[0])).then(|| f[0].to_string())
    })
}

/// The interface of the first usable IPv6 default route in
/// `/proc/net/ipv6_route` (destination ::/0).
pub fn default_route_v6(table: &str, up: &dyn Fn(&str) -> bool) -> Option<String> {
    table.lines().find_map(|line| {
        let f: Vec<&str> = line.split_whitespace().collect();
        if f.len() < 10 || f[9] == "lo" || f[1] != "00" || f[0].bytes().any(|b| b != b'0') {
            return None;
        }
        let flags = u32::from_str_radix(f[8], 16).ok()?;
        (flags & RTF_UP != 0 && flags & RTF_REJECT == 0 && up(f[9])).then(|| f[9].to_string())
    })
}

/// Where the route tables and interfaces are; a test tree in tests.
#[derive(Clone, Debug)]
pub struct Tables {
    pub proc_net: PathBuf,
    pub sys_net: PathBuf,
}

impl Default for Tables {
    fn default() -> Self {
        Self {
            proc_net: PathBuf::from("/proc/net"),
            sys_net: PathBuf::from("/sys/class/net"),
        }
    }
}

impl Tables {
    fn link_up(&self, iface: &str) -> bool {
        if iface.contains('/') || iface.starts_with('.') {
            return false;
        }
        let state =
            std::fs::read_to_string(self.sys_net.join(iface).join("operstate")).unwrap_or_default();
        matches!(state.trim(), "up" | "unknown")
    }

    /// The interface the machine is online through, or None when offline.
    pub fn online(&self) -> Option<String> {
        let up = |iface: &str| self.link_up(iface);
        let read =
            |name: &str| std::fs::read_to_string(self.proc_net.join(name)).unwrap_or_default();
        default_route_v4(&read("route"), &up).or_else(|| default_route_v6(&read("ipv6_route"), &up))
    }
}

/// The decisions, separate from the socket so they can be tested.
#[derive(Debug)]
pub struct Tracker {
    online: bool,
    /// When the current state (online or not) began.
    since: Instant,
    /// The connection was up at some point; only then can it be lost.
    seen_online: bool,
    announced_down: bool,
    quiet_until: Instant,
    last_up: Option<Instant>,
}

impl Tracker {
    pub fn new(online: bool, now: Instant) -> Self {
        Self {
            online,
            since: now,
            seen_online: online,
            announced_down: false,
            quiet_until: now + START_QUIET,
            last_up: None,
        }
    }

    pub fn observe(&mut self, online: bool, now: Instant) {
        if online != self.online {
            self.online = online;
            self.since = now;
        }
        self.seen_online |= online;
    }

    /// After a resume an outage counts from now, and is quiet for a while.
    pub fn resumed(&mut self, now: Instant) {
        self.since = now;
        self.quiet_until = self.quiet_until.max(now + RESUME_QUIET);
    }

    fn down_due(&self) -> Option<Instant> {
        if self.online || self.announced_down || !self.seen_online {
            return None;
        }
        let mut due = (self.since + DOWN_AFTER).max(self.quiet_until);
        if let Some(up) = self.last_up {
            due = due.max(up + FLAP_QUIET);
        }
        Some(due)
    }

    fn up_due(&self) -> Option<Instant> {
        (self.online && self.announced_down).then(|| self.since + UP_AFTER)
    }

    /// When `poll` may next have something to say.
    pub fn deadline(&self) -> Option<Instant> {
        self.down_due().or_else(|| self.up_due())
    }

    pub fn poll(&mut self, now: Instant) -> Option<&'static str> {
        if self.down_due().is_some_and(|due| now >= due) {
            self.announced_down = true;
            return Some("network-down");
        }
        if self.up_due().is_some_and(|due| now >= due) {
            self.announced_down = false;
            self.last_up = Some(now);
            return Some("network-up");
        }
        None
    }
}

fn open_route_netlink() -> Option<RawFd> {
    const RTMGRP_LINK: u32 = 0x1;
    const RTMGRP_IPV4_ROUTE: u32 = 0x40;
    const RTMGRP_IPV6_ROUTE: u32 = 0x400;
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_ROUTE,
        )
    };
    if fd < 0 {
        return None;
    }
    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as libc::sa_family_t;
    addr.nl_groups = RTMGRP_LINK | RTMGRP_IPV4_ROUTE | RTMGRP_IPV6_ROUTE;
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

/// For `nirunotify --check-config`.
pub fn describe() -> String {
    let online = Tables::default().online();
    let events = open_route_netlink()
        .map(|fd| {
            unsafe { libc::close(fd) };
            "ok"
        })
        .unwrap_or("unavailable");
    format!(
        "network: {} rtnetlink={events}",
        online
            .map(|iface| format!("online via {iface}"))
            .unwrap_or_else(|| "offline".into())
    )
}

/// Start the watcher. Off in tests that use a fake sysfs (they must not hear
/// the real machine) and with NIRUNOTIFY_NETWORK=off.
pub fn start(sounds: Sender<Request>) {
    if std::env::var_os("NIRUNOTIFY_SYSFS").is_some()
        || std::env::var("NIRUNOTIFY_NETWORK").as_deref() == Ok("off")
    {
        return;
    }
    let _ = std::thread::Builder::new()
        .name("nirunotify-network".into())
        .spawn(move || run(sounds, Tables::default()));
}

fn drain(fd: RawFd, buffer: &mut [u8]) {
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
            continue;
        }
        // ENOBUFS (events were lost) needs nothing more: the tables are read
        // again below. EAGAIN means the queue is empty.
        if read < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ENOBUFS) {
            continue;
        }
        break;
    }
}

fn run(sounds: Sender<Request>, tables: Tables) {
    let Some(fd) = open_route_netlink() else {
        return;
    };
    let mut buffer = vec![0u8; 16384];
    let mut tracker = Tracker::new(tables.online().is_some(), Instant::now());
    let mut suspended = crate::uevents::suspended_seconds();
    loop {
        let timeout = tracker
            .deadline()
            .map(|d| {
                d.saturating_duration_since(Instant::now())
                    .as_millis()
                    .clamp(1, 60_000) as libc::c_int
            })
            .unwrap_or(-1);
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut pfd, 1, timeout) };
        if ready < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if ready > 0 {
            drain(fd, &mut buffer);
        }
        let now = Instant::now();
        let slept = crate::uevents::suspended_seconds();
        if slept - suspended > 1.0 {
            tracker.resumed(now);
        }
        suspended = slept;
        let online = tables.online();
        tracker.observe(online.is_some(), now);
        if let Some(event) = tracker.poll(now) {
            debug(&format!("{event} (default route: {online:?})"));
            let _ = sounds.send(Request::new(event));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V4: &str =
        "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n\
        tailscale0\t0064A8C0\t00000000\t0001\t0\t0\t0\t00FFFFFF\t0\t0\t0\n\
        wlan0\t00000000\t0101A8C0\t0003\t0\t0\t600\t00000000\t0\t0\t0\n";
    const V6: &str = "00000000000000000000000000000000 00 00000000000000000000000000000000 00 00000000000000000000000000000000 ffffffff 00000001 00000000 00200200       lo\n\
        fd7a115ca1e000000000000000000053 80 00000000000000000000000000000000 00 00000000000000000000000000000000 00000400 00000001 00000000 00000001 tailscale0\n\
        00000000000000000000000000000000 00 00000000000000000000000000000000 00 fe800000000000000000000000000001 00000400 00000001 00000000 00450003 eth0\n";

    #[test]
    fn default_routes_are_found_and_need_a_live_link() {
        let all = |_: &str| true;
        let none = |_: &str| false;
        assert_eq!(default_route_v4(V4, &all).as_deref(), Some("wlan0"));
        assert_eq!(default_route_v4(V4, &none), None);
        assert_eq!(default_route_v6(V6, &all).as_deref(), Some("eth0"));
        // Only the reject route on lo and a non-default route: offline.
        let v6: String = V6.lines().take(2).map(|l| format!("{l}\n")).collect();
        assert_eq!(default_route_v6(&v6, &all), None);
        // A route that is not up does not count.
        let down = V4.replace("\t0003\t", "\t0002\t");
        assert_eq!(default_route_v4(&down, &all), None);
        assert_eq!(default_route_v4("", &all), None);
    }

    #[test]
    fn tables_read_proc_and_operstate() {
        let root = std::env::temp_dir().join(format!("nirunotify-net-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let tables = Tables {
            proc_net: root.join("proc"),
            sys_net: root.join("sys"),
        };
        std::fs::create_dir_all(&tables.proc_net).unwrap();
        std::fs::create_dir_all(tables.sys_net.join("wlan0")).unwrap();
        std::fs::write(tables.proc_net.join("route"), V4).unwrap();
        std::fs::write(tables.sys_net.join("wlan0/operstate"), "up\n").unwrap();
        assert_eq!(tables.online().as_deref(), Some("wlan0"));
        std::fs::write(tables.sys_net.join("wlan0/operstate"), "down\n").unwrap();
        assert_eq!(tables.online(), None);
        std::fs::write(tables.sys_net.join("wlan0/operstate"), "unknown\n").unwrap();
        assert_eq!(tables.online().as_deref(), Some("wlan0"));
        let _ = std::fs::remove_dir_all(root);
    }

    fn s(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_real_outage_is_announced_once_and_its_end_too() {
        let t = Instant::now();
        let mut k = Tracker::new(true, t);
        k.observe(false, t + s(60));
        assert_eq!(k.deadline(), Some(t + s(66)));
        assert_eq!(k.poll(t + s(65)), None);
        assert_eq!(k.poll(t + s(66)), Some("network-down"));
        assert_eq!(k.poll(t + s(90)), None, "only once");
        assert_eq!(k.deadline(), None);
        k.observe(true, t + s(100));
        assert_eq!(k.poll(t + s(102)), None);
        assert_eq!(k.poll(t + s(103)), Some("network-up"));
        assert_eq!(k.poll(t + s(200)), None);
    }

    #[test]
    fn short_drops_and_the_session_start_are_silent() {
        let t = Instant::now();
        // Starting offline and coming up: nothing to say.
        let mut k = Tracker::new(false, t);
        assert_eq!(k.poll(t + s(100)), None);
        k.observe(true, t + s(5));
        assert_eq!(k.poll(t + s(100)), None);
        // A drop shorter than DOWN_AFTER is silent.
        k.observe(false, t + s(200));
        k.observe(true, t + s(204));
        assert_eq!(k.poll(t + s(210)), None);
        // A drop in the first seconds of the session waits for START_QUIET.
        let mut k = Tracker::new(true, t);
        k.observe(false, t + s(2));
        assert_eq!(k.poll(t + s(10)), None);
        k.observe(true, t + s(20));
        assert_eq!(k.poll(t + s(40)), None);
    }

    #[test]
    fn a_resume_is_quiet_while_wifi_reconnects() {
        let t = Instant::now();
        let mut k = Tracker::new(true, t);
        k.observe(false, t + s(100));
        k.resumed(t + s(3000));
        assert_eq!(k.poll(t + s(3001)), None, "the time asleep does not count");
        k.observe(true, t + s(3010));
        assert_eq!(k.poll(t + s(3030)), None);
        // Still gone well after the resume: that is a real outage.
        let mut k = Tracker::new(true, t);
        k.observe(false, t + s(100));
        k.resumed(t + s(3000));
        assert_eq!(k.poll(t + s(3019)), None);
        assert_eq!(k.poll(t + s(3020)), Some("network-down"));
    }

    #[test]
    fn a_flapping_link_is_one_pair_of_sounds() {
        let t = Instant::now();
        let mut k = Tracker::new(true, t);
        k.observe(false, t + s(60));
        assert_eq!(k.poll(t + s(66)), Some("network-down"));
        k.observe(true, t + s(70));
        // Up for a second only: no network-up yet.
        k.observe(false, t + s(71));
        k.observe(true, t + s(72));
        assert_eq!(k.poll(t + s(75)), Some("network-up"));
        k.observe(false, t + s(76));
        assert_eq!(k.poll(t + s(90)), None, "flap quiet");
        assert_eq!(k.deadline(), Some(t + s(105)));
        assert_eq!(k.poll(t + s(105)), Some("network-down"));
    }
}
