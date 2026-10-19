//! NIRUNOTIFY — native org.freedesktop.Notifications service for NIRU.
//!
//! D-Bus owns the protocol and history; the X11 thread owns every visible
//! surface. Notifications are override-redirect windows so ordinary messages
//! never steal keyboard focus. The session lock suppresses and queues private
//! content until it is established that the lock is gone.

mod config;
mod render;

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
        let reply = match command {
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
                let channel = parts.next().unwrap_or("").to_string();
                let text = parts.next().unwrap_or("").to_string();
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
            _ => "commands: dnd, history, clear, dismiss ID, osd CHANNEL TEXT, visible\n".into(),
        };
        let _ = stream.write_all(reply.as_bytes());
    }
}

/// The monitor selected by config: pointer, RandR primary or a named output.
fn monitor_area(c: &RustConnection, root: Window, choice: &str) -> (i16, i16, u16, u16) {
    let setup = c.setup();
    let screen = &setup.roots[0];
    let fallback = (0, 0, screen.width_in_pixels, screen.height_in_pixels);
    let Ok(resources) = c.randr_get_screen_resources_current(root) else {
        return fallback;
    };
    let Ok(resources) = resources.reply() else {
        return fallback;
    };
    let primary = c
        .randr_get_output_primary(root)
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.output)
        .unwrap_or(0);
    let (px, py) = c
        .query_pointer(root)
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| (r.root_x, r.root_y))
        .unwrap_or((-1, -1));
    let mut pointer_hit = None;
    for output in resources.outputs {
        let Ok(info) = c.randr_get_output_info(output, resources.config_timestamp) else {
            continue;
        };
        let Ok(info) = info.reply() else { continue };
        if info.connection != x11rb::protocol::randr::Connection::CONNECTED || info.crtc == 0 {
            continue;
        }
        let Ok(crtc) = c.randr_get_crtc_info(info.crtc, resources.config_timestamp) else {
            continue;
        };
        let Ok(crtc) = crtc.reply() else { continue };
        if crtc.width == 0 || crtc.height == 0 {
            continue;
        }
        let name = String::from_utf8_lossy(&info.name).to_string();
        let rect = (crtc.x, crtc.y, crtc.width, crtc.height);
        if !choice.is_empty() && choice != "primary" && name == choice {
            return rect;
        }
        if choice == "primary" && output == primary {
            return rect;
        }
        if px >= crtc.x
            && px < crtc.x + crtc.width as i16
            && py >= crtc.y
            && py < crtc.y + crtc.height as i16
        {
            pointer_hit = Some(rect);
        }
    }
    pointer_hit.unwrap_or(fallback)
}

/// Space reserved by docks (the bar) at the top and bottom of the root.
///
/// The bar publishes `_NET_WM_STRUT_PARTIAL`; notifications start below it so
/// they never sit flush against the bar. Both strut properties are accepted
/// for panels that only publish the old four-value form.
fn reserved_edges(c: &RustConnection, root: Window) -> (u16, u16) {
    let Ok(tree) = c.query_tree(root) else {
        return (0, 0);
    };
    let Ok(tree) = tree.reply() else {
        return (0, 0);
    };
    let partial = c
        .intern_atom(false, b"_NET_WM_STRUT_PARTIAL")
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.atom);
    let basic = c
        .intern_atom(false, b"_NET_WM_STRUT")
        .ok()
        .and_then(|r| r.reply().ok())
        .map(|r| r.atom);
    let mut top = 0u16;
    let mut bottom = 0u16;
    for window in tree.children {
        for atom in [partial, basic].into_iter().flatten() {
            let Ok(cookie) = c.get_property(false, window, atom, AtomEnum::CARDINAL, 0, 12) else {
                continue;
            };
            let Ok(reply) = cookie.reply() else { continue };
            let Some(values) = reply.value32() else {
                continue;
            };
            let values: Vec<u32> = values.collect();
            if values.len() >= 4 {
                top = top.max(values[2].min(u32::from(u16::MAX)) as u16);
                bottom = bottom.max(values[3].min(u32::from(u16::MAX)) as u16);
            }
        }
    }
    (top, bottom)
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
}

impl Popup {
    fn destroy(self, c: &RustConnection) {
        let _ = self.renderer.release(c);
        let _ = c.destroy_window(self.win);
        let _ = c.free_gc(self.gc);
    }
}

struct Ui {
    c: RustConnection,
    root: Window,
    cfg: Config,
    rx: Receiver<UiCommand>,
    events: Sender<UiEvent>,
    popups: Vec<Popup>,
    osd: Option<Popup>,
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
}

impl Ui {
    fn new(cfg: Config, rx: Receiver<UiCommand>, events: Sender<UiEvent>) -> Result<Self> {
        let (c, screen) = x11rb::connect(None)?;
        let root = c.setup().roots[screen].root;
        let geometry = c.get_geometry(root)?.reply()?;
        let lock_atom = c.intern_atom(false, b"_NIRU_LOCK_READY")?.reply()?.atom;
        let (origin_x, origin_y, area_w, area_h) = monitor_area(&c, root, &cfg.monitor);
        let (reserved_top, reserved_bottom) = reserved_edges(&c, root);
        Ok(Self {
            c,
            root,
            cfg,
            rx,
            events,
            popups: Vec::new(),
            osd: None,
            locked: false,
            pending: Vec::new(),
            lock_atom,
            last_lock_check: Instant::now() - Duration::from_secs(1),
            height_limit: area_h.min(geometry.height),
            width_limit: area_w.min(geometry.width),
            origin_x,
            origin_y,
            reserved_top,
            reserved_bottom,
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
        let max_chars = ((self.cfg.width as i16 - padding * 2) / 8).max(12) as usize;
        let lines = wrap(text, max_chars, 8);
        let line_h = 20u16;
        let app_h = if id.is_some() { line_h } else { 0 };
        let height = (padding as u16 * 2
            + line_h
            + lines.len() as u16 * line_h
            + app_h
            + actions.len() as u16 * line_h)
            .min(self.height_limit.saturating_sub(20))
            .max(40);
        let win = self.c.generate_id()?;
        let gc = self.c.generate_id()?;
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
        self.c.create_window(
            0,
            win,
            self.root,
            x,
            y,
            self.cfg.width,
            height,
            0,
            WindowClass::INPUT_OUTPUT,
            0,
            &CreateWindowAux::new()
                .override_redirect(1)
                .background_pixel(self.cfg.palette.surface)
                .event_mask(EventMask::EXPOSURE | EventMask::BUTTON_PRESS),
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
        self.c.map_window(win)?;
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
        })
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
                0 => pal.error,
                2 => pal.warning,
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
            baseline += 20;
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
            baseline += 20;
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
            baseline += 20;
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
                20,
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
            self.c
                .configure_window(p.win, &ConfigureWindowAux::new().x(x as i32).y(y as i32))?;
            y += p.height as i16 + self.cfg.margin;
        }
        self.c.flush()?;
        Ok(())
    }

    fn show(&mut self, notification: Notification) -> Result<()> {
        let timeout = if notification.timeout_ms == u64::MAX {
            if notification.urgency == 2 {
                self.cfg.critical_timeout_ms
            } else {
                self.cfg.default_timeout_ms
            }
        } else if notification.timeout_ms == 0 {
            self.cfg.max_timeout_ms
        } else {
            notification.timeout_ms.min(self.cfg.max_timeout_ms)
        };
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
            if let Some(old) = self.popups.pop() {
                old.destroy(&self.c);
            }
        }
        for i in 0..self.popups.len() {
            self.draw_popup(i)?;
        }
        self.layout()
    }

    fn osd(&mut self, _channel: &str, text: &str) -> Result<()> {
        if let Some(old) = self.osd.take() {
            old.destroy(&self.c);
        }
        let mut popup = self.create_popup(None, text, self.cfg.palette.accent, &[])?;
        popup.text = text.to_string();
        // OSD is a single centred box, not part of the stack.
        let x = self.origin_x + (self.width_limit as i16 - self.cfg.width as i16) / 2;
        let usable =
            self.height_limit as i16 - self.reserved_top as i16 - self.reserved_bottom as i16;
        let y = self.origin_y + self.reserved_top as i16 + (usable - popup.height as i16) / 2;
        self.c.configure_window(
            popup.win,
            &ConfigureWindowAux::new().x(x as i32).y(y as i32),
        )?;
        self.draw_osd(&mut popup)?;
        self.osd = Some(popup);
        self.c.flush()?;
        Ok(())
    }

    fn draw_osd(&self, popup: &mut Popup) -> Result<()> {
        let pal = self.cfg.palette;
        popup.renderer.clear(&self.c, popup.gc, pal.surface)?;
        let baseline = popup.height as i16 / 2 + popup.renderer.line_height / 2 - 2;
        popup.renderer.line(
            &self.c,
            popup.gc,
            16,
            baseline,
            &popup.text,
            pal.foreground,
            pal.surface,
        )?;
        popup
            .renderer
            .present(&self.c, popup.gc, popup.win, self.cfg.width)?;
        Ok(())
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
            UiCommand::Close(id) => self.close(id, 2, true),
            UiCommand::CloseClient(id) => self.close(id, 0, false),
            UiCommand::Osd { channel, text } => self.osd(&channel, &text),
        }
    }

    fn hide_for_lock(&mut self) -> Result<()> {
        for p in self.popups.drain(..) {
            p.destroy(&self.c);
        }
        if let Some(p) = self.osd.take() {
            p.destroy(&self.c);
        }
        self.c.flush()?;
        Ok(())
    }

    fn run(&mut self) -> Result<()> {
        loop {
            while let Ok(command) = self.rx.try_recv() {
                if self.locked {
                    if self.pending.len() >= 100 {
                        self.pending.remove(0);
                    }
                    self.pending.push(command);
                } else {
                    self.handle_command(command)?;
                }
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
                        } else if self.osd.as_ref().is_some_and(|p| p.win == e.window) {
                            if let Some(mut p) = self.osd.take() {
                                self.draw_osd(&mut p)?;
                                self.osd = Some(p);
                            }
                        }
                    }
                    Event::Error(e) => eprintln!("nirunotify: X11 error: {e:?}"),
                    _ => {}
                }
            }
            self.expire()?;
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
                thread::sleep(Duration::from_millis(25));
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

fn daemon() -> Result<()> {
    let cfg = Config::load()?;
    let (ui_tx, ui_rx) = mpsc::channel();
    let (event_tx, event_rx) = mpsc::channel();
    let shared = Arc::new(Mutex::new(Shared {
        next_id: 1,
        notifications: HashMap::new(),
        history: VecDeque::new(),
        dnd: false,
        critical_ignore_dnd: cfg.critical_ignore_dnd,
        ui: ui_tx,
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
        "--check-config" => Config::load().map(|c| println!("{}", c.describe())),
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
        "--osd" => match args.get(1) {
            Some(channel) => {
                let text = args[2..].join(" ");
                control(&format!("osd {channel} {text}")).map(|r| print!("{r}"))
            }
            None => Err("--osd needs CHANNEL TEXT".into()),
        },
        "" => daemon(),
        "--help" => {
            println!("Usage: nirunotify [--dnd on|off|toggle|status|--history|--count|--clear|--dismiss ID|--osd CHANNEL TEXT|--check-config|--version]");
            Ok(())
        }
        _ => {
            eprintln!("Usage: nirunotify [--dnd on|off|toggle|status|--history|--count|--clear|--dismiss ID|--osd CHANNEL TEXT|--check-config|--version]");
            std::process::exit(2);
        }
    };
    if let Err(e) = result {
        eprintln!("nirunotify: {e}");
        std::process::exit(1);
    }
}
