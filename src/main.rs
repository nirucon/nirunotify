//! NIRUNOTIFY — native org.freedesktop.Notifications service for NIRU.
//!
//! D-Bus owns the protocol and history; the X11 thread owns every visible
//! surface. Notifications are override-redirect windows so ordinary messages
//! never steal keyboard focus. The session lock suppresses and queues private
//! content until it is established that the lock is gone.

mod config;
mod osd;
mod render;
mod sounds;
mod uevents;

use config::{Config, Position};
use render::Renderer;
use std::{
    collections::{HashMap, VecDeque},
    io::{Read, Write},
    os::unix::net::{UnixListener, UnixStream},
    path::PathBuf,
    sync::{
        mpsc::{self, Receiver, Sender},
        Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};
use x11rb::{
    connection::Connection,
    protocol::{randr::ConnectionExt as _, xproto::*, Event},
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};
use zbus::{blocking::ConnectionBuilder, interface, zvariant::OwnedValue};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

const BUS_NAME: &str = "org.freedesktop.Notifications";
const OBJECT_PATH: &str = "/org/freedesktop/Notifications";

#[derive(Clone, Debug)]
struct Notification {
    id: u32,
    app: String,
    summary: String,
    body: String,
    urgency: u8,
    timeout_ms: u64,
    actions: Vec<(String, String)>,
}

#[derive(Clone)]
enum UiCommand {
    Show(Notification),
    Silent(Notification),
    Close(u32),
    CloseClient(u32),
    Osd { channel: String, text: String },
}

enum UiEvent {
    Closed { id: u32, reason: u32 },
    ClientClose(u32),
    Action { id: u32, key: String },
}

struct Shared {
    next_id: u32,
    notifications: HashMap<u32, Notification>,
    history: VecDeque<Notification>,
    dnd: bool,
    critical_ignore_dnd: bool,
    ui: Sender<UiCommand>,
    sound: Sender<String>,
    /// Feedback, power, device and lock-screen sounds, played natively.
    feedback: Sender<sounds::Request>,
    /// Injected test uevents (NIRUNOTIFY_TEST_UEVENTS=1 only).
    test_uevents: Option<std::fs::File>,
}

struct NotifyIface {
    shared: Arc<Mutex<Shared>>,
    events: Sender<UiEvent>,
}

#[interface(name = "org.freedesktop.Notifications")]
impl NotifyIface {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &mut self,
        app_name: &str,
        replaces_id: u32,
        _app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> u32 {
        let urgency = hints
            .get("urgency")
            .and_then(|v| u8::try_from(v).ok())
            .unwrap_or(1)
            .min(2);
        let actions: Vec<(String, String)> = actions
            .chunks_exact(2)
            .take(4)
            .filter(|pair| !pair[0].is_empty())
            .map(|pair| (truncate(&pair[0], 64), truncate(&pair[1], 80)))
            .collect();
        let timeout_ms = if expire_timeout < 0 {
            None
        } else {
            Some(expire_timeout as u64)
        };
        let mut shared = self.shared.lock().unwrap();
        let id = if replaces_id != 0 && shared.notifications.contains_key(&replaces_id) {
            replaces_id
        } else {
            let id = shared.next_id.max(1);
            shared.next_id = id.wrapping_add(1).max(1);
            id
        };
        let notification = Notification {
            id,
            app: truncate(app_name, 100),
            summary: truncate(summary, 200),
            body: truncate(body, 4000),
            urgency,
            timeout_ms: timeout_ms.unwrap_or(u64::MAX),
            actions,
        };
        shared.notifications.insert(id, notification.clone());
        shared.history.push_back(notification.clone());
        while shared.history.len() > 100 {
            if let Some(old) = shared.history.front().map(|n| n.id) {
                shared.notifications.remove(&old);
            }
            shared.history.pop_front();
        }
        // Even a client that never closes a suppressed notification cannot
        // grow the live map without bound.
        while shared.notifications.len() > 100 {
            let Some(old_id) = shared.notifications.keys().copied().min() else {
                break;
            };
            shared.notifications.remove(&old_id);
        }
        if !shared.dnd || (urgency == 2 && shared.critical_ignore_dnd) {
            let _ = shared.ui.send(UiCommand::Show(notification));
            play_notification_sound(&shared.sound, app_name);
        } else {
            // DND hides the popup, but the notification still needs a bounded
            // lifetime so the client receives NotificationClosed instead of
            // waiting forever for a window that will never appear.
            let _ = shared.ui.send(UiCommand::Silent(notification));
        }
        id
    }

    fn close_notification(&mut self, id: u32) {
        let _ = self.events.send(UiEvent::ClientClose(id));
        let shared = self.shared.lock().unwrap();
        let _ = shared.ui.send(UiCommand::CloseClient(id));
    }

    fn get_capabilities(&self) -> Vec<String> {
        vec!["body".into(), "actions".into()]
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "NIRUNOTIFY".into(),
            "NIRU".into(),
            env!("CARGO_PKG_VERSION").into(),
            "1.2".into(),
        )
    }
}

/// One worker owns every notification sound. It coalesces bursts, then spawns
/// `nirux` and waits for it, so a notification can never leave a zombie behind.
const SOUND_COALESCE: Duration = Duration::from_millis(200);

fn start_sound_worker() -> Sender<String> {
    let (tx, rx) = mpsc::channel::<String>();
    let _ = thread::Builder::new()
        .name("nirunotify-sound".into())
        .spawn(move || sound_worker(rx));
    tx
}

fn sound_worker(rx: Receiver<String>) {
    let mut last: Option<Instant> = None;
    while let Ok(mut app) = rx.recv() {
        // A notification burst is one audible event; keep only the newest app
        // filter instead of starting one process and audio stream per item.
        while let Ok(next) = rx.try_recv() {
            app = next;
        }
        if let Some(at) = last {
            let wait = SOUND_COALESCE.saturating_sub(at.elapsed());
            if !wait.is_zero() {
                thread::sleep(wait);
                while let Ok(next) = rx.try_recv() {
                    app = next;
                }
            }
        }
        if let Ok(mut child) = std::process::Command::new("nirux")
            .args(["sounds", "play", "notification", "--app", &app])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            let _ = child.wait();
        }
        last = Some(Instant::now());
    }
}

fn play_notification_sound(sound: &Sender<String>, app: &str) {
    let _ = sound.send(app.to_string());
}

fn truncate(text: &str, max: usize) -> String {
    text.chars().take(max).collect()
}

fn socket_path() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join(format!("nirunotify-{}.sock", unsafe { libc::getuid() }))
}

fn control(cmd: &str) -> Result<String> {
    let mut stream =
        UnixStream::connect(socket_path()).map_err(|_| "NIRUNOTIFY daemon is not running")?;
    stream.write_all(cmd.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reply = String::new();
    stream.read_to_string(&mut reply)?;
    Ok(reply)
}

fn router(
    connection: zbus::blocking::Connection,
    shared: Arc<Mutex<Shared>>,
    events: Receiver<UiEvent>,
) {
    while let Ok(event) = events.recv() {
        match event {
            UiEvent::Closed { id, reason } => {
                shared.lock().unwrap().notifications.remove(&id);
                let _ = connection.emit_signal(
                    None::<&str>,
                    OBJECT_PATH,
                    BUS_NAME,
                    "NotificationClosed",
                    &(id, reason),
                );
            }
            UiEvent::ClientClose(id) => {
                shared.lock().unwrap().notifications.remove(&id);
                let _ = connection.emit_signal(
                    None::<&str>,
                    OBJECT_PATH,
                    BUS_NAME,
                    "NotificationClosed",
                    &(id, 3u32),
                );
            }
            UiEvent::Action { id, key } => {
                shared.lock().unwrap().notifications.remove(&id);
                let _ = connection.emit_signal(
                    None::<&str>,
                    OBJECT_PATH,
                    BUS_NAME,
                    "ActionInvoked",
                    &(id, key.as_str()),
                );
                let _ = connection.emit_signal(
                    None::<&str>,
                    OBJECT_PATH,
                    BUS_NAME,
                    "NotificationClosed",
                    &(id, 2u32),
                );
            }
        }
    }
}

fn control_server(shared: Arc<Mutex<Shared>>) {
    let path = socket_path();
    let _ = std::fs::remove_file(&path);
    let Ok(listener) = UnixListener::bind(&path) else {
        eprintln!("nirunotify: cannot bind {}", path.display());
        return;
    };
    let _ = std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600));
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        let mut line = String::new();
        let mut byte = [0u8; 1];
        loop {
            match stream.read(&mut byte) {
                Ok(0) => break,
                Ok(_) if byte[0] == b'\n' => break,
                Ok(_) => line.push(byte[0] as char),
                Err(_) => break,
            }
        }
        let mut parts = line.trim().splitn(4, ' ');
        let command = parts.next().unwrap_or("");
        let rest = line.trim().split_once(' ').map(|(_, r)| r).unwrap_or("");
        let reply = match command {
            "sound" => match sounds::Request::parse(rest) {
                Some(request) => {
                    let _ = shared.lock().unwrap().feedback.send(request);
                    "ok\n".into()
                }
                None => "sound needs a known EVENT [LEVEL] [muted|unmuted]\n".into(),
            },
            "uevent" => {
                let mut state = shared.lock().unwrap();
                match state.test_uevents.as_mut() {
                    Some(pipe) => {
                        let _ = pipe.write_all(format!("{rest}\n").as_bytes());
                        "ok\n".into()
                    }
                    None => "uevent injection is off\n".into(),
                }
            }
            "dnd" => {
                let action = parts.next().unwrap_or("status");
                let mut state = shared.lock().unwrap();
                match action {
                    "on" => {
                        state.dnd = true;
                        "dnd on".into()
                    }
                    "off" => {
                        state.dnd = false;
                        "dnd off".into()
                    }
                    "toggle" => {
                        state.dnd = !state.dnd;
                        if state.dnd { "dnd on" } else { "dnd off" }.into()
                    }
                    "status" => if state.dnd { "dnd on" } else { "dnd off" }.into(),
                    _ => "dnd needs on, off, toggle or status".into(),
                }
            }
            "history" => {
                let state = shared.lock().unwrap();
                let mut out = String::new();
                for n in &state.history {
                    out.push_str(&format!(
                        "{}\t{}\t{}\t{}\n",
                        n.id,
                        n.app,
                        n.summary,
                        n.body.replace('\n', " ")
                    ));
                }
                out
            }
            "clear" => {
                shared.lock().unwrap().history.clear();
                "history cleared\n".into()
            }
            "dismiss" => {
                let id = parts.next().and_then(|v| v.parse::<u32>().ok());
                match id {
                    Some(id) => {
                        let _ = shared.lock().unwrap().ui.send(UiCommand::Close(id));
                        format!("dismissed {id}\n")
                    }
                    None => "dismiss needs an id\n".into(),
                }
            }
            "osd" => {
                // `osd CHANNEL TEXT…`: the text may contain spaces.
                let (channel, text) = rest.split_once(' ').unwrap_or((rest, ""));
                let (channel, text) = (channel.trim().to_string(), text.trim().to_string());
                if channel.is_empty() {
                    "osd needs CHANNEL TEXT\n".into()
                } else {
                    let _ = shared
                        .lock()
                        .unwrap()
                        .ui
                        .send(UiCommand::Osd { channel, text });
                    "ok\n".into()
                }
            }
            "visible" => {
                let n = shared.lock().unwrap().notifications.len();
                format!("{n}\n")
            }
            _ => "commands: dnd, history, clear, dismiss ID, osd CHANNEL TEXT, sound EVENT [LEVEL], visible\n".into(),
        };
        let _ = stream.write_all(reply.as_bytes());
    }
}

/// A monitor rectangle in root coordinates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Area {
    x: i16,
    y: i16,
    w: u16,
    h: u16,
}

impl Area {
    fn contains(&self, px: i16, py: i16) -> bool {
        i32::from(px) >= i32::from(self.x)
            && i32::from(px) < i32::from(self.x) + i32::from(self.w)
            && i32::from(py) >= i32::from(self.y)
            && i32::from(py) < i32::from(self.y) + i32::from(self.h)
    }
}

/// Active monitors as (name, area, is primary).
///
/// RandR 1.5 monitors come first: they are what the server presents as
/// screens (and what test servers can define). Older servers fall back to
/// connected outputs with an active CRTC.
fn outputs(c: &RustConnection, root: Window) -> Vec<(String, Area, bool)> {
    let monitors = c
        .randr_get_monitors(root, true)
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.monitors)
        .unwrap_or_default();
    let mut out: Vec<(String, Area, bool)> = monitors
        .iter()
        .filter(|m| m.width > 0 && m.height > 0)
        .map(|m| {
            let name = c
                .get_atom_name(m.name)
                .ok()
                .and_then(|r| r.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).to_string())
                .unwrap_or_default();
            (
                name,
                Area {
                    x: m.x,
                    y: m.y,
                    w: m.width,
                    h: m.height,
                },
                m.primary,
            )
        })
        .collect();
    if !out.is_empty() {
        return out;
    }
    let Some(resources) = c
        .randr_get_screen_resources_current(root)
        .ok()
        .and_then(|r| r.reply().ok())
    else {
        return out;
    };
    let primary = c
        .randr_get_output_primary(root)
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.output)
        .unwrap_or(0);
    for output in resources.outputs {
        let Some(info) = c
            .randr_get_output_info(output, resources.config_timestamp)
            .ok()
            .and_then(|r| r.reply().ok())
        else {
            continue;
        };
        if info.connection != x11rb::protocol::randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        let Some(crtc) = c
            .randr_get_crtc_info(info.crtc, resources.config_timestamp)
            .ok()
            .and_then(|r| r.reply().ok())
        else {
            continue;
        };
        if crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        out.push((
            String::from_utf8_lossy(&info.name).to_string(),
            Area {
                x: crtc.x,
                y: crtc.y,
                w: crtc.width,
                h: crtc.height,
            },
            output == primary,
        ));
    }
    out
}

/// The output NIRUWM has focused, from its published monitor table.
///
/// `_NIRUWM_MONITORS` lists one line per screen: name, x, y, width, first
/// global workspace, workspace count, active workspace and layout.
/// `_NET_CURRENT_DESKTOP` is the global index of the focused workspace, so
/// the focused screen is the one whose workspace range contains it. Returns
/// the name and the published origin.
fn focused_in_table(table: &str, current: u32) -> Option<(String, i16, i16)> {
    for line in table.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() < 6 {
            continue;
        }
        let (Ok(x), Ok(y), Ok(start), Ok(count)) = (
            f[1].parse::<i16>(),
            f[2].parse::<i16>(),
            f[4].parse::<u32>(),
            f[5].parse::<u32>(),
        ) else {
            continue;
        };
        if current >= start && current < start.saturating_add(count.max(1)) {
            return Some((f[0].to_string(), x, y));
        }
    }
    None
}

fn intern(c: &RustConnection, name: &[u8]) -> Option<Atom> {
    c.intern_atom(false, name)
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.atom)
}

fn focused_output(c: &RustConnection, root: Window) -> Option<(String, i16, i16)> {
    let monitors = intern(c, b"_NIRUWM_MONITORS")?;
    let current = intern(c, b"_NET_CURRENT_DESKTOP")?;
    let table = c
        .get_property(false, root, monitors, AtomEnum::ANY, 0, 4096)
        .ok()?
        .reply()
        .ok()?;
    let table = String::from_utf8_lossy(&table.value).to_string();
    let current = c
        .get_property(false, root, current, AtomEnum::CARDINAL, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()?;
    focused_in_table(&table, current)
}

/// Pick the monitor for the next surface.
///
/// `""`/`"focused"` follows the NIRUWM focused screen, which is where the
/// user is working even when the pointer rests on another screen; without a
/// NIRUWM table it falls back to the pointer, then the RandR primary.
/// `"pointer"`, `"primary"` or an output name select explicitly. The choice
/// is made again for every surface, so it follows focus, hotplug and docking.
fn monitor_area(c: &RustConnection, root: Window, choice: &str) -> Area {
    let screen = &c.setup().roots[0];
    let fallback = Area {
        x: 0,
        y: 0,
        w: screen.width_in_pixels,
        h: screen.height_in_pixels,
    };
    let outs = outputs(c, root);
    if outs.is_empty() {
        return fallback;
    }
    let pointer = || {
        let (px, py) = c
            .query_pointer(root)
            .ok()
            .and_then(|r| r.reply().ok())
            .map(|r| (r.root_x, r.root_y))?;
        outs.iter()
            .find(|(_, a, _)| a.contains(px, py))
            .map(|(_, a, _)| *a)
    };
    let primary = || outs.iter().find(|(_, _, p)| *p).map(|(_, a, _)| *a);
    let named = |name: &str| outs.iter().find(|(n, _, _)| n == name).map(|(_, a, _)| *a);
    let chosen = match choice {
        "" | "focused" => focused_output(c, root)
            .and_then(|(name, x, y)| {
                named(&name).or_else(|| {
                    outs.iter()
                        .find(|(_, a, _)| a.x == x && a.y == y)
                        .map(|(_, a, _)| *a)
                })
            })
            .or_else(pointer),
        "pointer" => pointer(),
        "primary" => primary(),
        name => named(name),
    };
    chosen
        .or_else(primary)
        .or_else(|| outs.first().map(|(_, a, _)| *a))
        .unwrap_or(fallback)
}

/// Space a dock (the bar) takes at the top and bottom of one monitor.
///
/// Struts are relative to the root edges, so with screens stacked vertically
/// the lower screen's bar says nothing useful about either screen. The dock
/// windows' own geometry does: a mapped dock inside the monitor reserves the
/// band from the monitor edge it sits nearest to.
fn reserved_edges(c: &RustConnection, root: Window, area: Area) -> (u16, u16) {
    let Some(tree) = c.query_tree(root).ok().and_then(|t| t.reply().ok()) else {
        return (0, 0);
    };
    let atoms: Vec<Atom> = [&b"_NET_WM_STRUT_PARTIAL"[..], &b"_NET_WM_STRUT"[..]]
        .into_iter()
        .filter_map(|name| intern(c, name))
        .collect();
    let mut top = 0u16;
    let mut bottom = 0u16;
    for window in tree.children {
        let has_strut = atoms.iter().any(|&atom| {
            c.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 12)
                .ok()
                .and_then(|r| r.reply().ok())
                .and_then(|r| r.value32().map(|v| v.take(4).any(|n| n > 0)))
                .unwrap_or(false)
        });
        if !has_strut {
            continue;
        }
        let viewable = c
            .get_window_attributes(window)
            .ok()
            .and_then(|r| r.reply().ok())
            .is_some_and(|a| a.map_state == MapState::VIEWABLE);
        let Some(g) = c.get_geometry(window).ok().and_then(|r| r.reply().ok()) else {
            continue;
        };
        if let Some((t, b)) = dock_band(area, viewable, g.x, g.y, g.width, g.height) {
            top = top.max(t);
            bottom = bottom.max(b);
        }
    }
    (top, bottom)
}

/// The (top, bottom) band one dock window reserves on a monitor, if any.
fn dock_band(area: Area, viewable: bool, x: i16, y: i16, w: u16, h: u16) -> Option<(u16, u16)> {
    if !viewable || w == 0 || h == 0 || h >= area.h / 2 {
        return None;
    }
    let cx = i32::from(x) + i32::from(w) / 2;
    let cy = i32::from(y) + i32::from(h) / 2;
    let (ax, ay) = (i32::from(area.x), i32::from(area.y));
    let (aw, ah) = (i32::from(area.w), i32::from(area.h));
    if cx < ax || cx >= ax + aw || cy < ay || cy >= ay + ah {
        return None;
    }
    if cy < ay + ah / 2 {
        let band = (i32::from(y) + i32::from(h) - ay).clamp(0, ah) as u16;
        Some((band, 0))
    } else {
        let band = (ay + ah - i32::from(y)).clamp(0, ah) as u16;
        Some((0, band))
    }
}

struct Popup {
    id: Option<u32>,
    win: Window,
    gc: Gcontext,
    renderer: Renderer,
    height: u16,
    expires: Instant,
    notification: Option<Notification>,
    text: String,
    actions: Vec<(String, String)>,
    /// Action key and its clickable rectangle inside the popup.
    hitboxes: Vec<(String, i16, i16, u16, u16)>,
    last_raise: Instant,
}

impl Popup {
    fn destroy(self, c: &RustConnection) {
        let _ = self.renderer.release(c);
        let _ = c.destroy_window(self.win);
        let _ = c.free_gc(self.gc);
    }

    fn raise(&mut self, c: &RustConnection) -> Result<()> {
        c.configure_window(
            self.win,
            &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
        )?;
        self.last_raise = Instant::now();
        Ok(())
    }
}

/// The hardware OSD: one box with a label, the value and a level bar.
///
/// It is kept and redrawn while keys repeat instead of being recreated per
/// press, so a held key glides the bar without the window flickering.
struct Osd {
    win: Window,
    gc: Gcontext,
    renderer: Renderer,
    width: u16,
    height: u16,
    metrics: osd::Metrics,
    channel: String,
    text: String,
    level: Option<u32>,
    muted: bool,
    /// Bar level when the current glide started, and its target.
    from: f32,
    target: f32,
    started: Instant,
    flash_until: Option<Instant>,
    expires: Instant,
    /// Whether the last frame was drawn mid-glide or lit up.
    moving: bool,
}

impl Osd {
    fn destroy(self, c: &RustConnection) {
        let _ = self.renderer.release(c);
        let _ = c.destroy_window(self.win);
        let _ = c.free_gc(self.gc);
    }

    /// The level the bar shows right now.
    fn shown(&self, now: Instant) -> f32 {
        let t =
            now.saturating_duration_since(self.started).as_secs_f32() / osd::GLIDE.as_secs_f32();
        osd::glide(self.from, self.target, t)
    }

    fn animating(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.started) < osd::GLIDE
            || self.flash_until.is_some_and(|until| now < until)
    }
}

struct Ui {
    c: RustConnection,
    root: Window,
    cfg: Config,
    rx: Receiver<UiCommand>,
    events: Sender<UiEvent>,
    popups: Vec<Popup>,
    silent: Vec<(u32, Instant)>,
    osd: Option<Osd>,
    /// Last level per OSD channel, so `Muted` still shows how loud it was.
    osd_levels: HashMap<String, u32>,
    locked: bool,
    pending: Vec<UiCommand>,
    lock_atom: Atom,
    last_lock_check: Instant,
    height_limit: u16,
    width_limit: u16,
    origin_x: i16,
    origin_y: i16,
    reserved_top: u16,
    reserved_bottom: u16,
    config_serial: Atom,
    theme_atom: Atom,
    window_type_atom: Atom,
    window_state_atom: Atom,
    notification_type_atom: Atom,
    above_state_atom: Atom,
}

impl Ui {
    fn new(cfg: Config, rx: Receiver<UiCommand>, events: Sender<UiEvent>) -> Result<Self> {
        let (c, screen) = x11rb::connect(None)?;
        let root = c.setup().roots[screen].root;
        let geometry = c.get_geometry(root)?.reply()?;
        let lock_atom = c.intern_atom(false, b"_NIRU_LOCK_READY")?.reply()?.atom;
        let config_serial = c.intern_atom(false, b"_NIRU_CONFIG_SERIAL")?.reply()?.atom;
        let theme_atom = c.intern_atom(false, b"_NIRUWM_THEME")?.reply()?.atom;
        let window_type_atom = c.intern_atom(false, b"_NET_WM_WINDOW_TYPE")?.reply()?.atom;
        let window_state_atom = c.intern_atom(false, b"_NET_WM_STATE")?.reply()?.atom;
        let notification_type_atom = c
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE_NOTIFICATION")?
            .reply()?
            .atom;
        let above_state_atom = c.intern_atom(false, b"_NET_WM_STATE_ABOVE")?.reply()?.atom;
        // NIRUWM themes are applied with `niructl theme`, which bumps the
        // config serial; older managers also publish the theme name. Both
        // are root properties, so one mask covers reload and theme changes.
        c.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )?;
        let area = monitor_area(&c, root, &cfg.monitor);
        let (reserved_top, reserved_bottom) = reserved_edges(&c, root, area);
        // Output changes (hotplug, docking, nirudisplay profiles) move the
        // area under the surfaces; follow them instead of the start-up layout.
        let _ = c.randr_select_input(root, x11rb::protocol::randr::NotifyMask::SCREEN_CHANGE);
        Ok(Self {
            c,
            root,
            cfg,
            rx,
            events,
            popups: Vec::new(),
            silent: Vec::new(),
            osd: None,
            osd_levels: HashMap::new(),
            locked: false,
            pending: Vec::new(),
            lock_atom,
            last_lock_check: Instant::now() - Duration::from_secs(1),
            height_limit: area.h.min(geometry.height),
            width_limit: area.w.min(geometry.width),
            origin_x: area.x,
            origin_y: area.y,
            reserved_top,
            reserved_bottom,
            config_serial,
            theme_atom,
            window_type_atom,
            window_state_atom,
            notification_type_atom,
            above_state_atom,
        })
    }

    fn lock_established(&self) -> bool {
        let Ok(cookie) = self
            .c
            .get_property(false, self.root, self.lock_atom, AtomEnum::ANY, 0, 1)
        else {
            return false;
        };
        let Ok(reply) = cookie.reply() else {
            return false;
        };
        let Some(window) = reply.value32().and_then(|mut v| v.next()) else {
            return false;
        };
        self.c
            .get_window_attributes(window)
            .ok()
            .and_then(|r| r.reply().ok())
            .is_some_and(|a| a.map_state == MapState::VIEWABLE)
    }

    fn create_popup(
        &self,
        id: Option<u32>,
        text: &str,
        colour: u32,
        actions: &[(String, String)],
    ) -> Result<Popup> {
        let padding = 12i16;
        let (advance, line_height) =
            Renderer::measure(self.cfg.font_path.as_deref(), self.cfg.font_size)?;
        let line_step = (line_height + 6).max(12) as u16;
        let max_chars =
            ((self.cfg.width as i16 - padding * 2 - 6) / advance.max(1)).max(8) as usize;
        let lines = wrap(text, max_chars, 8);
        let rows = lines.len() as u16 + actions.len() as u16 + u16::from(id.is_some());
        let height =
            (padding as u16 * 2 + line_height as u16 + rows.saturating_sub(1) * line_step + 6)
                .min(self.height_limit.saturating_sub(20))
                .max(40);
        let x = self.origin_x
            + match self.cfg.position {
                Position::TopRight | Position::BottomRight => {
                    (self.width_limit as i16 - self.cfg.width as i16 - self.cfg.margin).max(0)
                }
                Position::TopLeft | Position::BottomLeft => self.cfg.margin,
            };
        let y = self.origin_y + self.reserved_top as i16 + self.cfg.margin;
        let renderer = Renderer::new(
            &self.c,
            self.root,
            self.cfg.font_path.as_deref(),
            self.cfg.font_size,
            self.cfg.width,
            height,
        )?;
        let (win, gc) = self.new_surface(x, y, self.cfg.width, height)?;
        let timeout = if id.is_some() {
            Duration::from_millis(
                self.cfg
                    .max_timeout_ms
                    .min(self.cfg.default_timeout_ms.max(500)),
            )
        } else {
            Duration::from_millis(1200)
        };
        let _ = colour;
        Ok(Popup {
            id,
            win,
            gc,
            renderer,
            height,
            expires: Instant::now() + timeout,
            notification: None,
            text: text.to_string(),
            actions: actions.to_vec(),
            hitboxes: Vec::new(),
            last_raise: Instant::now(),
        })
    }

    /// Create and map one override-redirect surface (notification or OSD).
    ///
    /// Override-redirect keeps keyboard focus where it is; the notification
    /// window type and the above state tell compositors and the bar what it is.
    fn new_surface(&self, x: i16, y: i16, width: u16, height: u16) -> Result<(Window, Gcontext)> {
        let win = self.c.generate_id()?;
        let gc = self.c.generate_id()?;
        self.c.create_window(
            0,
            win,
            self.root,
            x,
            y,
            width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(self.cfg.palette.surface)
                .event_mask(
                    EventMask::EXPOSURE | EventMask::BUTTON_PRESS | EventMask::VISIBILITY_CHANGE,
                ),
        )?;
        self.c.create_gc(gc, win, &CreateGCAux::new())?;
        self.c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_NAME,
            AtomEnum::STRING,
            b"NIRUNOTIFY",
        )?;
        self.c.change_property8(
            PropMode::REPLACE,
            win,
            AtomEnum::WM_CLASS,
            AtomEnum::STRING,
            b"nirunotify\0NIRUNOTIFY\0",
        )?;
        self.c.change_property32(
            PropMode::REPLACE,
            win,
            self.window_type_atom,
            AtomEnum::ATOM,
            &[self.notification_type_atom],
        )?;
        self.c.change_property32(
            PropMode::REPLACE,
            win,
            self.window_state_atom,
            AtomEnum::ATOM,
            &[self.above_state_atom],
        )?;
        self.c.map_window(win)?;
        self.c
            .configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE))?;
        Ok((win, gc))
    }

    fn draw_popup(&mut self, index: usize) -> Result<()> {
        let padding = 12i16;
        let pal = self.cfg.palette;
        let (win, gc, height, text, notification, renderer_height) = {
            let p = &self.popups[index];
            (
                p.win,
                p.gc,
                p.height,
                p.text.clone(),
                p.notification.clone(),
                p.height,
            )
        };
        let _ = renderer_height;
        // Recreate renderer if the height changed is handled by replacing the
        // popup; the renderer always matches `height` here.
        let p = &mut self.popups[index];
        p.renderer.clear(&self.c, gc, pal.surface)?;
        p.renderer.fill(
            &self.c,
            gc,
            Rectangle {
                x: 0,
                y: 0,
                width: 4,
                height,
            },
            match notification.as_ref().map(|n| n.urgency).unwrap_or(1) {
                0 => pal.muted,
                2 => pal.error,
                _ => pal.accent,
            },
        )?;
        let title = notification
            .as_ref()
            .map(|n| n.summary.clone())
            .unwrap_or_else(|| text.clone());
        let body = notification
            .as_ref()
            .map(|n| n.body.clone())
            .unwrap_or_default();
        let app = notification
            .as_ref()
            .map(|n| n.app.clone())
            .unwrap_or_default();
        let line_step = (p.renderer.line_height + 6).max(12);
        let mut baseline = padding + p.renderer.line_height;
        p.renderer.line(
            &self.c,
            gc,
            padding + 6,
            baseline,
            &title,
            pal.foreground,
            pal.surface,
        )?;
        for line in wrap(
            &body,
            ((self.cfg.width as i16 - padding * 2 - 6) / p.renderer.advance.max(1)) as usize,
            8,
        ) {
            baseline += line_step;
            p.renderer.line(
                &self.c,
                gc,
                padding + 6,
                baseline,
                &line,
                pal.muted,
                pal.surface,
            )?;
        }
        if !app.is_empty() {
            baseline += line_step;
            p.renderer.line(
                &self.c,
                gc,
                padding + 6,
                baseline,
                &app,
                pal.muted,
                pal.surface,
            )?;
        }
        p.hitboxes.clear();
        let action_count = p.actions.len();
        for (index, (key, label)) in p.actions.iter().enumerate() {
            baseline += line_step;
            let text = format!("[ {label} ]");
            p.renderer.line(
                &self.c,
                gc,
                padding + 6,
                baseline,
                &text,
                if key == "default" {
                    pal.accent
                } else {
                    pal.foreground
                },
                pal.surface,
            )?;
            let top = baseline - p.renderer.line_height + 2;
            p.hitboxes.push((
                key.clone(),
                padding + 4,
                top,
                self.cfg.width.saturating_sub((padding * 2 + 4) as u16),
                line_step as u16,
            ));
            if index + 1 == action_count {
                break;
            }
        }
        p.renderer.present(&self.c, gc, win, self.cfg.width)?;
        self.c.flush()?;
        Ok(())
    }

    fn layout(&mut self) -> Result<()> {
        let mut y = self.origin_y + self.reserved_top as i16 + self.cfg.margin;
        let total: i16 = self
            .popups
            .iter()
            .map(|p| p.height as i16 + self.cfg.margin)
            .sum();
        if matches!(
            self.cfg.position,
            Position::BottomRight | Position::BottomLeft
        ) {
            y = self.origin_y + self.height_limit as i16
                - self.reserved_bottom as i16
                - total
                - self.cfg.margin;
        }
        for p in &self.popups {
            let x = self.origin_x
                + match self.cfg.position {
                    Position::TopRight | Position::BottomRight => {
                        (self.width_limit as i16 - self.cfg.width as i16 - self.cfg.margin).max(0)
                    }
                    Position::TopLeft | Position::BottomLeft => self.cfg.margin,
                };
            self.c.configure_window(
                p.win,
                &ConfigureWindowAux::new()
                    .x(x as i32)
                    .y(y as i32)
                    .stack_mode(StackMode::ABOVE),
            )?;
            y += p.height as i16 + self.cfg.margin;
        }
        self.c.flush()?;
        Ok(())
    }

    /// Choose the monitor again (focus, pointer or output may have changed)
    /// and re-read the bar band on it. Returns whether the area moved.
    fn refresh_area(&mut self) -> bool {
        let area = monitor_area(&self.c, self.root, &self.cfg.monitor);
        let (top, bottom) = reserved_edges(&self.c, self.root, area);
        let (rw, rh) = self
            .c
            .get_geometry(self.root)
            .ok()
            .and_then(|r| r.reply().ok())
            .map(|g| (g.width, g.height))
            .unwrap_or((area.w, area.h));
        let moved = (
            self.origin_x,
            self.origin_y,
            self.width_limit,
            self.height_limit,
        ) != (area.x, area.y, area.w.min(rw), area.h.min(rh));
        self.origin_x = area.x;
        self.origin_y = area.y;
        self.width_limit = area.w.min(rw);
        self.height_limit = area.h.min(rh);
        self.reserved_top = top;
        self.reserved_bottom = bottom;
        moved
    }

    /// Re-read the shared NIRU configuration after a theme or reload command.
    /// A bad file keeps the last working palette, matching NIRUWM and NIRUBAR.
    fn reload_config(&mut self) -> Result<()> {
        let cfg = match Config::load() {
            Ok(cfg) => cfg,
            Err(e) => {
                eprintln!("nirunotify: reload rejected: {e}");
                return Ok(());
            }
        };
        if cfg == self.cfg {
            return Ok(());
        }
        let rebuild = cfg.width != self.cfg.width
            || cfg.margin != self.cfg.margin
            || cfg.max_visible != self.cfg.max_visible
            || cfg.monitor != self.cfg.monitor
            || cfg.font_path != self.cfg.font_path
            || cfg.font_size != self.cfg.font_size
            || cfg.position != self.cfg.position
            || cfg.palette != self.cfg.palette;
        if !rebuild {
            self.cfg = cfg;
            return Ok(());
        }
        self.apply_config(cfg)
    }

    fn apply_config(&mut self, cfg: Config) -> Result<()> {
        let mut notifications = Vec::new();
        for popup in self.popups.drain(..) {
            notifications.push((
                popup.id,
                popup.text.clone(),
                popup.actions.clone(),
                popup.notification.clone(),
                popup.expires,
            ));
            popup.destroy(&self.c);
        }
        let osd = self.osd.take().map(|o| {
            let state = (o.channel.clone(), o.text.clone(), o.expires);
            o.destroy(&self.c);
            state
        });
        self.cfg = cfg;
        self.refresh_area();
        for (id, text, actions, notification, expires) in notifications {
            let mut popup = self.create_popup(id, &text, self.cfg.palette.accent, &actions)?;
            popup.notification = notification;
            popup.expires = expires;
            self.popups.push(popup);
        }
        while self.popups.len() > self.cfg.max_visible {
            let Some(old) = self.popups.pop() else {
                break;
            };
            let id = old.id;
            old.destroy(&self.c);
            if let Some(id) = id {
                self.close(id, 4, true)?;
            }
        }
        for index in 0..self.popups.len() {
            self.draw_popup(index)?;
        }
        self.layout()?;
        if let Some((channel, text, expires)) = osd {
            self.osd(&channel, &text)?;
            if let Some(o) = self.osd.as_mut() {
                o.expires = expires;
            }
        }
        self.expire()
    }

    /// Put the OSD on the current monitor, low and centred.
    fn place_osd(&self, o: &Osd) -> Result<()> {
        let x = self.origin_x + (self.width_limit as i16 - o.width as i16).max(0) / 2;
        let y = osd::top(
            self.origin_y,
            self.height_limit,
            self.reserved_top,
            self.reserved_bottom,
            o.height,
        );
        self.c.configure_window(
            o.win,
            &ConfigureWindowAux::new()
                .x(i32::from(x))
                .y(i32::from(y))
                .stack_mode(StackMode::ABOVE),
        )?;
        Ok(())
    }

    fn timeout_for(&self, notification: &Notification) -> u64 {
        if notification.timeout_ms == u64::MAX {
            if notification.urgency == 2 {
                self.cfg.critical_timeout_ms
            } else {
                self.cfg.default_timeout_ms
            }
        } else if notification.timeout_ms == 0 {
            self.cfg.max_timeout_ms
        } else {
            notification.timeout_ms.min(self.cfg.max_timeout_ms)
        }
    }

    fn silence(&mut self, notification: Notification) {
        let timeout = self.timeout_for(&notification);
        let id = notification.id;
        self.silent.retain(|(other, _)| *other != id);
        self.silent
            .push((id, Instant::now() + Duration::from_millis(timeout)));
    }

    fn show(&mut self, notification: Notification) -> Result<()> {
        // The bar may map after the notifier starts. Re-read its strut so a
        // notification never starts flush against a newly started bar.
        self.refresh_area();
        let timeout = self.timeout_for(&notification);
        self.silent.retain(|(id, _)| *id != notification.id);
        if let Some(index) = self
            .popups
            .iter()
            .position(|p| p.id == Some(notification.id))
        {
            let old = self.popups.remove(index);
            old.destroy(&self.c);
        }
        let text = format!("{}\n{}", notification.summary, notification.body);
        let actions = notification.actions.clone();
        let mut popup = self.create_popup(
            Some(notification.id),
            &text,
            self.cfg.palette.accent,
            &actions,
        )?;
        popup.notification = Some(notification);
        popup.expires = Instant::now() + Duration::from_millis(timeout);
        self.popups.insert(0, popup);
        while self.popups.len() > self.cfg.max_visible {
            let Some(old) = self.popups.pop() else {
                break;
            };
            let id = old.id;
            old.destroy(&self.c);
            if let Some(id) = id {
                self.close(id, 4, true)?;
            }
        }
        for i in 0..self.popups.len() {
            self.draw_popup(i)?;
        }
        self.layout()
    }

    fn osd(&mut self, channel: &str, text: &str) -> Result<()> {
        // Every press picks the monitor again, so the OSD follows focus.
        self.refresh_area();
        let now = Instant::now();
        let (level, muted, shown_text) = match osd::read(text) {
            osd::Reading::Level(n) => {
                self.osd_levels.insert(channel.to_string(), n);
                (Some(n), false, format!("{n}%"))
            }
            osd::Reading::Muted => (
                self.osd_levels.get(channel).copied(),
                true,
                "Muted".to_string(),
            ),
            osd::Reading::Text(t) => (None, false, t),
        };
        let metrics = osd::metrics(self.cfg.font_size);
        // A different channel or a font change starts a fresh box; a repeat
        // of the same channel keeps the window and glides from where it is.
        let keep = self
            .osd
            .as_ref()
            .is_some_and(|o| o.channel == channel && o.metrics == metrics);
        if !keep {
            if let Some(old) = self.osd.take() {
                old.destroy(&self.c);
            }
            let line = Renderer::measure(self.cfg.font_path.as_deref(), metrics.big)?.1;
            let height = (metrics.padding * 2 + line + metrics.gap) as u16 + metrics.bar;
            let width = metrics
                .width
                .min(self.width_limit.saturating_sub(16))
                .max(160);
            let (win, gc) = self.new_surface(self.origin_x, self.origin_y, width, height)?;
            let renderer = Renderer::new(
                &self.c,
                self.root,
                self.cfg.font_path.as_deref(),
                self.cfg.font_size,
                width,
                height,
            )?;
            let start = level.map(|l| l as f32).unwrap_or(0.0);
            self.osd = Some(Osd {
                win,
                gc,
                renderer,
                width,
                height,
                metrics,
                channel: channel.to_string(),
                text: String::new(),
                level,
                muted,
                from: start,
                target: start,
                started: now - osd::GLIDE,
                flash_until: None,
                expires: now,
                moving: false,
            });
        }
        let Some(mut o) = self.osd.take() else {
            return Ok(());
        };
        let shown = o.shown(now);
        // Pressing against the end of the range lights the bar briefly, so a
        // key that cannot go further still answers.
        if keep
            && level.is_some()
            && level == o.level
            && matches!(level, Some(0) | Some(100))
            && !muted
        {
            o.flash_until = Some(now + osd::FLASH);
        }
        if let Some(l) = level {
            o.from = shown;
            o.target = l as f32;
            o.started = now;
        }
        o.level = level;
        o.muted = muted;
        o.text = shown_text;
        o.expires = now + osd::LINGER;
        self.place_osd(&o)?;
        self.draw_osd(&mut o, now)?;
        self.osd = Some(o);
        self.c.flush()?;
        Ok(())
    }

    /// Draw the whole OSD: frame, icon, label, value and bar.
    fn draw_osd(&self, o: &mut Osd, now: Instant) -> Result<()> {
        let pal = self.cfg.palette;
        let m = o.metrics;
        o.renderer.clear(&self.c, o.gc, pal.border)?;
        o.renderer.fill(
            &self.c,
            o.gc,
            Rectangle {
                x: 1,
                y: 1,
                width: o.width.saturating_sub(2),
                height: o.height.saturating_sub(2),
            },
            pal.surface,
        )?;
        let big_ascent = o.renderer.ascent_at(m.big);
        let baseline = m.padding + big_ascent;
        let mut x = m.padding;
        if let Some(icon) =
            osd::icon(&o.channel, o.level, o.muted).filter(|&i| o.renderer.has_glyph(i))
        {
            o.renderer.line_at_size(
                &self.c,
                o.gc,
                x,
                baseline,
                &icon.to_string(),
                m.big,
                if o.muted {
                    pal.muted
                } else {
                    pal.accent_text()
                },
                pal.surface,
            )?;
            let ink = o.renderer.ink_width(icon, m.big);
            x += ink.max(o.renderer.advance_at(m.big)) + m.gap * 2 / 3;
        }
        o.renderer.line(
            &self.c,
            o.gc,
            x,
            baseline,
            &osd::label(&o.channel),
            pal.muted,
            pal.surface,
        )?;
        let chars = o.text.chars().count() as i16;
        let value_x = o.width as i16 - m.padding - chars * o.renderer.advance_at(m.big);
        o.renderer.line_at_size(
            &self.c,
            o.gc,
            value_x.max(x),
            baseline,
            &o.text,
            m.big,
            if o.muted { pal.muted } else { pal.foreground },
            pal.surface,
        )?;
        self.draw_osd_bar(o, now)
    }

    /// Draw only the bar and copy the frame; used for every glide frame.
    fn draw_osd_bar(&self, o: &mut Osd, now: Instant) -> Result<()> {
        let pal = self.cfg.palette;
        let m = o.metrics;
        let track_w = o.width.saturating_sub((m.padding * 2) as u16);
        let y = o.height as i16 - m.padding - m.bar as i16;
        let track = Rectangle {
            x: m.padding,
            y,
            width: track_w,
            height: m.bar,
        };
        if o.level.is_some() {
            o.renderer.fill(&self.c, o.gc, track, pal.track())?;
            let colour = if o.muted {
                pal.muted
            } else if o.flash_until.is_some_and(|until| now < until) {
                pal.foreground
            } else {
                pal.accent_text()
            };
            let fill = osd::fill_width(track_w, o.shown(now));
            o.renderer.fill(
                &self.c,
                o.gc,
                Rectangle {
                    width: fill,
                    ..track
                },
                colour,
            )?;
        } else {
            o.renderer.fill(&self.c, o.gc, track, pal.surface)?;
        }
        o.moving = o.animating(now);
        o.renderer.present(&self.c, o.gc, o.win, o.width)?;
        Ok(())
    }

    /// Advance a running glide or flash by one frame.
    fn tick_osd(&mut self) -> Result<()> {
        let now = Instant::now();
        let Some(mut o) = self.osd.take() else {
            return Ok(());
        };
        if o.moving {
            self.draw_osd_bar(&mut o, now)?;
            self.c.flush()?;
        }
        self.osd = Some(o);
        Ok(())
    }

    fn osd_moving(&self) -> bool {
        self.osd.as_ref().is_some_and(|o| o.moving)
    }

    fn activate_command(&mut self, id: u32, key: &str) -> Result<()> {
        if let Some(index) = self.popups.iter().position(|p| p.id == Some(id)) {
            let popup = self.popups.remove(index);
            popup.destroy(&self.c);
            self.layout()?;
        }
        let _ = self.events.send(UiEvent::Action {
            id,
            key: key.to_string(),
        });
        Ok(())
    }

    fn close(&mut self, id: u32, reason: u32, notify: bool) -> Result<()> {
        self.silent.retain(|(silent, _)| *silent != id);
        if let Some(index) = self.popups.iter().position(|p| p.id == Some(id)) {
            let popup = self.popups.remove(index);
            popup.destroy(&self.c);
            self.layout()?;
        }
        if notify {
            let _ = self.events.send(UiEvent::Closed { id, reason });
        }
        Ok(())
    }

    fn expire(&mut self) -> Result<()> {
        let now = Instant::now();
        let expired: Vec<u32> = self
            .popups
            .iter()
            .filter(|p| p.id.is_some() && now >= p.expires)
            .filter_map(|p| p.id)
            .collect();
        for id in expired {
            self.close(id, 1, true)?;
        }
        let silent: Vec<u32> = self
            .silent
            .iter()
            .filter(|(_, expires)| now >= *expires)
            .map(|(id, _)| *id)
            .collect();
        for id in silent {
            self.close(id, 1, true)?;
        }
        if self.osd.as_ref().is_some_and(|p| now >= p.expires) {
            if let Some(p) = self.osd.take() {
                p.destroy(&self.c);
            }
        }
        Ok(())
    }

    fn handle_command(&mut self, command: UiCommand) -> Result<()> {
        match command {
            UiCommand::Show(n) => self.show(n),
            UiCommand::Silent(n) => {
                self.silence(n);
                Ok(())
            }
            UiCommand::Close(id) => self.close(id, 2, true),
            UiCommand::CloseClient(id) => self.close(id, 0, false),
            UiCommand::Osd { channel, text } => self.osd(&channel, &text),
        }
    }

    fn hide_for_lock(&mut self) -> Result<()> {
        for popup in self.popups.drain(..) {
            let id = popup.id;
            popup.destroy(&self.c);
            if let Some(id) = id {
                let _ = self.events.send(UiEvent::Closed { id, reason: 4 });
            }
        }
        if let Some(p) = self.osd.take() {
            p.destroy(&self.c);
        }
        self.c.flush()?;
        Ok(())
    }

    fn accept_command(&mut self, command: UiCommand) -> Result<()> {
        // A command can wake the channel immediately after the screen locked,
        // before the periodic lock check runs. Never show it over the lock.
        if !self.locked && self.lock_established() {
            self.locked = true;
            self.last_lock_check = Instant::now();
            self.hide_for_lock()?;
        }
        if self.locked {
            if self.pending.len() >= 100 {
                self.pending.remove(0);
            }
            self.pending.push(command);
            Ok(())
        } else {
            self.handle_command(command)
        }
    }

    fn run(&mut self) -> Result<()> {
        loop {
            while let Ok(command) = self.rx.try_recv() {
                self.accept_command(command)?;
            }
            let mut handled = false;
            while let Some(event) = self.c.poll_for_event()? {
                handled = true;
                match event {
                    Event::ButtonPress(e) => {
                        if let Some(index) = self.popups.iter().position(|p| p.win == e.event) {
                            let key = self.popups[index]
                                .hitboxes
                                .iter()
                                .find(|(_, x, y, w, h)| {
                                    e.event_x >= *x
                                        && e.event_y >= *y
                                        && e.event_x < x.saturating_add(*w as i16)
                                        && e.event_y < y.saturating_add(*h as i16)
                                })
                                .map(|(key, _, _, _, _)| key.clone())
                                .or_else(|| {
                                    self.popups[index].notification.as_ref().and_then(|n| {
                                        n.actions
                                            .iter()
                                            .find(|(key, _)| key == "default")
                                            .map(|(key, _)| key.clone())
                                    })
                                });
                            let id = self.popups[index].id.unwrap_or(0);
                            if let Some(key) = key {
                                self.activate_command(id, &key)?;
                            } else {
                                self.close(id, 2, true)?;
                            }
                        } else if self.osd.as_ref().is_some_and(|p| p.win == e.event) {
                            if let Some(p) = self.osd.take() {
                                p.destroy(&self.c);
                            }
                        }
                    }
                    Event::Expose(e) => {
                        if let Some(index) = self.popups.iter().position(|p| p.win == e.window) {
                            self.draw_popup(index)?;
                        } else if self.osd.as_ref().is_some_and(|o| o.win == e.window) {
                            if let Some(mut o) = self.osd.take() {
                                self.draw_osd(&mut o, Instant::now())?;
                                self.osd = Some(o);
                            }
                        }
                    }
                    Event::VisibilityNotify(e) => {
                        if let Some(index) = self.popups.iter().position(|p| p.win == e.window) {
                            let popup = &mut self.popups[index];
                            if e.state != Visibility::UNOBSCURED
                                && popup.last_raise.elapsed() >= Duration::from_millis(100)
                            {
                                popup.raise(&self.c)?;
                            }
                        }
                    }
                    Event::RandrScreenChangeNotify(_) => {
                        // Docking, hotplug or a display profile: move what is
                        // shown onto the screen that now applies.
                        self.refresh_area();
                        self.layout()?;
                        if let Some(o) = self.osd.as_ref() {
                            self.place_osd(o)?;
                        }
                    }
                    Event::PropertyNotify(e)
                        if e.window == self.root
                            && (e.atom == self.config_serial || e.atom == self.theme_atom) =>
                    {
                        self.reload_config()?;
                    }
                    Event::Error(e) => eprintln!("nirunotify: X11 error: {e:?}"),
                    _ => {}
                }
            }
            self.expire()?;
            self.tick_osd()?;
            if self.last_lock_check.elapsed() >= Duration::from_millis(250) {
                self.last_lock_check = Instant::now();
                let locked = self.lock_established();
                if locked != self.locked {
                    self.locked = locked;
                    if locked {
                        self.hide_for_lock()?;
                    } else {
                        let pending = std::mem::take(&mut self.pending);
                        for command in pending {
                            self.handle_command(command)?;
                        }
                    }
                }
            }
            if !handled {
                // The command channel wakes immediately for new notifications
                // and OSDs. When nothing is displayed there is no need to poll
                // X forty times per second: keep the existing 250 ms lock check
                // cadence. Visible/expiring windows retain the short X event
                // and expiry cadence for clicks, stacking and close deadlines.
                let wait = if self.osd_moving() {
                    // A gliding bar draws at about 120 Hz; it lasts 140 ms.
                    Duration::from_millis(8)
                } else if self.popups.is_empty() && self.osd.is_none() && self.silent.is_empty() {
                    Duration::from_millis(250)
                } else {
                    Duration::from_millis(25)
                };
                match self.rx.recv_timeout(wait) {
                    Ok(command) => self.accept_command(command)?,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                        return Err("notification command channel closed".into());
                    }
                }
            }
        }
    }
}

fn wrap(text: &str, width: usize, max_lines: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for raw in text.lines() {
        let mut line = String::new();
        for word in raw.split_whitespace() {
            if line.is_empty() {
                line.push_str(word);
            } else if line.chars().count() + 1 + word.chars().count() <= width.max(8) {
                line.push(' ');
                line.push_str(word);
            } else {
                lines.push(std::mem::take(&mut line));
                line.push_str(word);
            }
            if lines.len() >= max_lines {
                return lines;
            }
        }
        if !line.is_empty() {
            lines.push(line);
        }
        if lines.len() >= max_lines {
            return lines;
        }
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn test_pipe() -> Option<(std::fs::File, std::fs::File)> {
    use std::os::unix::io::FromRawFd;
    let mut fds = [0 as libc::c_int; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return None;
    }
    Some(unsafe {
        (
            std::fs::File::from_raw_fd(fds[0]),
            std::fs::File::from_raw_fd(fds[1]),
        )
    })
}

/// `EVENT [LEVEL] [muted|unmuted]` from the command line.
fn sound_command(words: &[String]) -> Result<String> {
    let line = words.join(" ");
    if sounds::Request::parse(&line).is_none() {
        let known: Vec<&str> = sounds::EVENTS.iter().map(|(e, _, _)| *e).collect();
        return Err(format!(
            "--sound needs EVENT [LEVEL] [muted|unmuted]; events: {}",
            known.join(" ")
        )
        .into());
    }
    control(&format!("sound {line}"))
}

fn daemon() -> Result<()> {
    let cfg = Config::load()?;
    let feedback = sounds::start();
    // Tests feed uevents through a pipe instead of the kernel socket.
    let (test_reader, test_writer) =
        if std::env::var("NIRUNOTIFY_TEST_UEVENTS").as_deref() == Ok("1") {
            test_pipe().map_or((None, None), |(r, w)| (Some(r), Some(w)))
        } else {
            (None, None)
        };
    uevents::start(feedback.clone(), test_reader);
    let (ui_tx, ui_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let shared = Arc::new(Mutex::new(Shared {
        next_id: 1,
        notifications: HashMap::new(),
        history: VecDeque::new(),
        dnd: false,
        critical_ignore_dnd: cfg.critical_ignore_dnd,
        ui: ui_tx,
        sound: start_sound_worker(),
        feedback,
        test_uevents: test_writer,
    }));
    let iface = NotifyIface {
        shared: shared.clone(),
        events: event_tx.clone(),
    };
    let connection = match ConnectionBuilder::session()?
        .name(BUS_NAME)?
        .serve_at(OBJECT_PATH, iface)?
        .build()
    {
        Ok(connection) => connection,
        Err(e) => {
            return Err(format!("cannot own {BUS_NAME}: {e}").into());
        }
    };
    let router_connection = connection.clone();
    let router_shared = shared.clone();
    thread::spawn(move || router(router_connection, router_shared, event_rx));
    let control_shared = shared.clone();
    thread::spawn(move || control_server(control_shared));
    let mut ui = Ui::new(cfg, ui_rx, event_tx)?;
    ui.run()
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let arg = args.first().map(String::as_str).unwrap_or("");
    let result = match arg {
        "--version" => {
            println!("NIRUNOTIFY {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        "--check-config" => Config::load().map(|c| {
            println!("{}", c.describe());
            println!("{}", uevents::describe());
        }),
        "--dnd" => {
            let action = args.get(1).map(String::as_str).unwrap_or("status");
            control(&format!("dnd {action}")).map(|r| print!("{r}"))
        }
        "--history" => control("history").map(|r| print!("{r}")),
        "--count" => control("visible").map(|r| print!("{}", r.trim())),
        "--clear" => control("clear").map(|r| print!("{r}")),
        "--dismiss" => match args.get(1) {
            Some(id) => control(&format!("dismiss {id}")).map(|r| print!("{r}")),
            None => Err("--dismiss needs an id".into()),
        },
        // `--osd CHANNEL TEXT… [--sound EVENT [LEVEL] [muted|unmuted]]`: one
        // process shows the OSD and plays the matching feedback sound.
        "--osd" => match args.get(1) {
            Some(channel) => {
                let rest = &args[2..];
                let split = rest
                    .iter()
                    .position(|a| a == "--sound")
                    .unwrap_or(rest.len());
                let text = rest[..split].join(" ");
                control(&format!("osd {channel} {text}")).and_then(|r| {
                    if split < rest.len() {
                        sound_command(&rest[split + 1..])?;
                    }
                    print!("{r}");
                    Ok(())
                })
            }
            None => Err("--osd needs CHANNEL TEXT".into()),
        },
        "--sound" => sound_command(&args[1..]).map(|r| print!("{r}")),
        // For tests only: the daemon accepts it with NIRUNOTIFY_TEST_UEVENTS=1.
        "--test-uevent" => control(&format!("uevent {}", args[1..].join(" "))).and_then(|r| {
            if r.starts_with("ok") {
                Ok(())
            } else {
                Err(r.trim().to_string().into())
            }
        }),
        "" => daemon(),
        "--help" => {
            println!("Usage: nirunotify [--dnd on|off|toggle|status|--history|--count|--clear|--dismiss ID|--osd CHANNEL TEXT [--sound EVENT [LEVEL]]|--sound EVENT [LEVEL]|--check-config|--version]");
            Ok(())
        }
        _ => {
            eprintln!("Usage: nirunotify [--dnd on|off|toggle|status|--history|--count|--clear|--dismiss ID|--osd CHANNEL TEXT [--sound EVENT [LEVEL]]|--sound EVENT [LEVEL]|--check-config|--version]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("nirunotify: {e}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod placement_tests {
    use super::*;

    const STACKED: &str = "DisplayPort-5\t0\t0\t2560\t0\t10\t0\ttile\n\
                           DisplayPort-4\t0\t1440\t2560\t10\t10\t2\ttile\n";

    #[test]
    fn focused_screen_comes_from_the_workspace_range() {
        assert_eq!(
            focused_in_table(STACKED, 3),
            Some(("DisplayPort-5".into(), 0, 0))
        );
        assert_eq!(
            focused_in_table(STACKED, 12),
            Some(("DisplayPort-4".into(), 0, 1440))
        );
        assert_eq!(focused_in_table(STACKED, 40), None);
        assert_eq!(focused_in_table("garbage\n", 0), None);
    }

    #[test]
    fn a_bar_reserves_only_its_own_screen() {
        let upper = Area {
            x: 0,
            y: 0,
            w: 2560,
            h: 1440,
        };
        let lower = Area {
            x: 0,
            y: 1440,
            w: 2560,
            h: 1440,
        };
        // The lower screen's top bar.
        assert_eq!(dock_band(lower, true, 0, 1440, 2560, 28), Some((28, 0)));
        assert_eq!(dock_band(upper, true, 0, 1440, 2560, 28), None);
        // A bottom bar on the upper screen.
        assert_eq!(dock_band(upper, true, 0, 1412, 2560, 28), Some((0, 28)));
        // A hidden bar reserves nothing.
        assert_eq!(dock_band(lower, false, 0, 1440, 2560, 28), None);
    }
}
