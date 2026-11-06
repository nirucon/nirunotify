//! NIRUX system sounds: feedback, power, device and lock-screen events.
//!
//! The settings live in `~/.config/nirux/sounds.toml`, which belongs to NIRUX
//! (`nirux sounds …`). This module reads it with the same rules as the
//! `nirux` shell script, so both always agree on what plays:
//!
//! - `[sounds] enabled`, the category's `enabled` and the event's own key must
//!   all be true. The event key is read from `[sounds.CATEGORY]` first, then
//!   from `[sounds]` (where startup, update, shutdown and notification have
//!   always lived), then from the built-in default.
//! - Keys before the first table header count as `[sounds]`; keys compare
//!   case-insensitively; the first occurrence of a key wins.
//! - `~/.config/nirux/sounds/EVENT.ogg` replaces the NIRUX sound for EVENT.
//!
//! Playing never goes through a shell: the daemon starts `pw-play` (or
//! `paplay`, `ffplay`) on the resolved file and reaps it. Nothing here
//! touches audio routing, sink volumes or other streams.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc::{self, Receiver, RecvTimeoutError, Sender},
    thread,
    time::{Duration, Instant, SystemTime},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    System,
    Notifications,
    Feedback,
    Power,
    Devices,
}

impl Category {
    pub fn name(self) -> &'static str {
        match self {
            Category::System => "system",
            Category::Notifications => "notifications",
            Category::Feedback => "feedback",
            Category::Power => "power",
            Category::Devices => "devices",
        }
    }
}

/// Every event, its category and whether it is on by default.
pub const EVENTS: &[(&str, Category, bool)] = &[
    ("startup", Category::System, true),
    ("update", Category::System, true),
    ("shutdown", Category::System, true),
    ("auth-fail", Category::System, true),
    ("lock", Category::System, false),
    ("unlock", Category::System, false),
    ("notification", Category::Notifications, true),
    ("volume-up", Category::Feedback, true),
    ("volume-down", Category::Feedback, true),
    ("volume-limit", Category::Feedback, true),
    ("mute", Category::Feedback, true),
    ("unmute", Category::Feedback, true),
    ("brightness-up", Category::Feedback, false),
    ("brightness-down", Category::Feedback, false),
    ("screenshot", Category::Feedback, true),
    ("power-plug", Category::Power, true),
    ("power-unplug", Category::Power, true),
    ("battery-low", Category::Power, true),
    ("battery-critical", Category::Power, true),
    ("battery-full", Category::Power, false),
    ("device-connect", Category::Devices, true),
    ("device-disconnect", Category::Devices, true),
];

pub fn lookup(event: &str) -> Option<(Category, bool)> {
    EVENTS
        .iter()
        .find(|(name, _, _)| *name == event)
        .map(|(_, category, default)| (*category, *default))
}

/// Events that a held key repeats; they share the feedback rate limit.
pub fn is_key_repeat(event: &str) -> bool {
    matches!(
        event,
        "volume-up"
            | "volume-down"
            | "volume-limit"
            | "mute"
            | "unmute"
            | "brightness-up"
            | "brightness-down"
    )
}

/// The parsed sounds.toml: table name -> (lower-case key -> raw value).
#[derive(Clone, Debug, Default)]
pub struct Settings {
    tables: HashMap<String, Vec<(String, String)>>,
}

fn trim(s: &str) -> &str {
    s.trim_matches(|c: char| c == ' ' || c == '\t' || c == '\r')
}

impl Settings {
    /// The same line rules as `sounds_table_value` in `nirux`.
    pub fn parse(text: &str) -> Self {
        let mut tables: HashMap<String, Vec<(String, String)>> = HashMap::new();
        let mut section = "sounds".to_string();
        for line in text.lines() {
            let stripped = line.trim_start_matches([' ', '\t']);
            if let Some(rest) = stripped.strip_prefix('[') {
                let end = rest.rfind(']').unwrap_or(rest.len());
                section = trim(&rest[..end]).to_string();
                continue;
            }
            let Some(eq) = line.find('=') else { continue };
            let key = trim(&line[..eq]);
            let key = key.strip_prefix('"').unwrap_or(key);
            let key = key.strip_suffix('"').unwrap_or(key);
            if key.starts_with('#') {
                continue;
            }
            let mut value = trim(&line[eq + 1..]).to_string();
            if let Some(quoted) = value.strip_prefix('"') {
                value = match quoted.find('"') {
                    Some(q) => quoted[..q].to_string(),
                    None => quoted.to_string(),
                };
            } else if !value.starts_with('[') {
                if let Some(hash) = value.find('#') {
                    value = trim(&value[..hash]).to_string();
                }
            }
            tables
                .entry(section.clone())
                .or_default()
                .push((key.to_lowercase(), value));
        }
        Self { tables }
    }

    pub fn load(path: &Path) -> Self {
        std::fs::read_to_string(path)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    /// The first value of KEY in TABLE, empty values counting as missing.
    pub fn value(&self, table: &str, key: &str) -> Option<&str> {
        let key = key.to_lowercase();
        self.tables
            .get(table)?
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    fn number(&self, table: &str, key: &str, default: u64) -> u64 {
        self.value(table, key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    pub fn enabled(&self) -> bool {
        if std::env::var("NIRUX_SOUNDS").as_deref() == Ok("off") {
            return false;
        }
        self.value("sounds", "enabled").unwrap_or("true") == "true"
    }

    pub fn category_enabled(&self, category: Category) -> bool {
        self.value(&format!("sounds.{}", category.name()), "enabled") != Some("false")
    }

    /// The event's own switch, ignoring its category and `[sounds] enabled`.
    pub fn event_setting(&self, event: &str) -> bool {
        let Some((category, default)) = lookup(event) else {
            return false;
        };
        let value = self
            .value(&format!("sounds.{}", category.name()), event)
            .or_else(|| self.value("sounds", event));
        match value {
            Some(v) => v == "true",
            None => default,
        }
    }

    pub fn event_enabled(&self, event: &str) -> bool {
        let Some((category, _)) = lookup(event) else {
            return false;
        };
        self.enabled() && self.category_enabled(category) && self.event_setting(event)
    }

    /// 1..=100: the category's own `volume`, else `[sounds] volume`, else 35.
    pub fn volume_for(&self, event: &str) -> u8 {
        let own = lookup(event)
            .and_then(|(c, _)| self.value(&format!("sounds.{}", c.name()), "volume"))
            .and_then(|v| v.parse::<u64>().ok());
        let volume = own
            .or_else(|| self.value("sounds", "volume").and_then(|v| v.parse().ok()))
            .unwrap_or(35);
        volume.clamp(1, 100) as u8
    }

    pub fn respect_mute(&self) -> bool {
        self.value("sounds", "respect_mute").unwrap_or("true") == "true"
    }

    pub fn rate_limit(&self) -> Duration {
        Duration::from_millis(
            self.number("sounds.feedback", "rate_limit_ms", 80)
                .min(10_000),
        )
    }

    pub fn debounce(&self) -> Duration {
        Duration::from_millis(
            self.number("sounds.devices", "debounce_ms", 1500)
                .min(60_000),
        )
    }

    pub fn battery_low(&self) -> u8 {
        self.number("sounds.power", "battery_low_percent", 20)
            .min(100) as u8
    }

    pub fn battery_critical(&self) -> u8 {
        self.number("sounds.power", "battery_critical_percent", 5)
            .min(100) as u8
    }

    pub fn battery_full(&self) -> u8 {
        self.number("sounds.power", "battery_full_percent", 100)
            .clamp(1, 100) as u8
    }

    /// Device sources: usb, audio, displays (on) and bluetooth (off).
    pub fn device_source(&self, source: &str) -> bool {
        let default = source != "bluetooth";
        match self.value("sounds.devices", source) {
            Some(v) => v == "true",
            None => default,
        }
    }
}

/// Where the settings and sound files are.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config: PathBuf,
    pub user: PathBuf,
    pub nirux: PathBuf,
}

impl Paths {
    pub fn from_env() -> Self {
        let home = PathBuf::from(std::env::var_os("HOME").unwrap_or_default());
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| home.join(".config"));
        let prefix = std::env::var_os("NIRUX_PREFIX")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local"));
        Self {
            config: config_home.join("nirux/sounds.toml"),
            user: config_home.join("nirux/sounds"),
            nirux: prefix.join("share/nirux/sounds"),
        }
    }

    /// The user's EVENT.ogg, then the NIRUX tick for the nearest 10 % step
    /// (volume ticks with a level), then the NIRUX EVENT.ogg.
    pub fn file_for(&self, event: &str, level: Option<u32>) -> Option<PathBuf> {
        let own = self.user.join(format!("{event}.ogg"));
        if own.is_file() {
            return Some(own);
        }
        if let (Some(level), "volume-up" | "volume-down") = (level, event) {
            let step = ((level.min(100) + 5) / 10 * 10).min(100);
            let tick = self
                .nirux
                .join("levels")
                .join(format!("{event}-{step:03}.ogg"));
            if tick.is_file() {
                return Some(tick);
            }
        }
        let base = self.nirux.join(format!("{event}.ogg"));
        base.is_file().then_some(base)
    }
}

/// One play per event per window: a held key repeats faster than a tick.
#[derive(Default)]
pub struct RateLimit {
    last: HashMap<String, Instant>,
}

impl RateLimit {
    pub fn allow(&mut self, event: &str, now: Instant, window: Duration) -> bool {
        if let Some(at) = self.last.get(event) {
            if now.saturating_duration_since(*at) < window {
                return false;
            }
        }
        self.last.insert(event.to_string(), now);
        true
    }
}

/// A request to play an event. `level` is the new volume for the ticks;
/// `muted` is the sink state when the caller already knows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub event: String,
    pub level: Option<u32>,
    pub muted: Option<bool>,
}

impl Request {
    pub fn new(event: &str) -> Self {
        Self {
            event: event.to_string(),
            level: None,
            muted: None,
        }
    }

    /// `EVENT [LEVEL] [muted|unmuted]`, as sent over the control socket.
    pub fn parse(text: &str) -> Option<Self> {
        let mut words = text.split_whitespace();
        let event = words.next()?;
        lookup(event)?;
        let mut request = Self::new(event);
        for word in words {
            match word {
                "muted" => request.muted = Some(true),
                "unmuted" => request.muted = Some(false),
                n => request.level = Some(n.trim_end_matches('%').parse().ok()?),
            }
        }
        Some(request)
    }
}

/// The settings, reloaded when sounds.toml changes on disk.
pub struct Cached {
    path: PathBuf,
    stamp: Option<(SystemTime, u64)>,
    pub settings: Settings,
}

impl Cached {
    pub fn new(path: PathBuf) -> Self {
        let mut cached = Self {
            path,
            stamp: None,
            settings: Settings::default(),
        };
        cached.refresh();
        cached
    }

    pub fn refresh(&mut self) -> &Settings {
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        if stamp != self.stamp || stamp.is_none() {
            self.settings = Settings::load(&self.path);
            self.stamp = stamp;
        }
        &self.settings
    }
}

/// With NIRUNOTIFY_DEBUG_SOUNDS=1, explain each decision on stderr.
pub fn debug(line: &str) {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *ON.get_or_init(|| std::env::var("NIRUNOTIFY_DEBUG_SOUNDS").as_deref() == Ok("1")) {
        eprintln!("nirunotify: sound: {line}");
    }
}

fn in_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
}

/// Whether the default sink is muted, from `wpctl`. Unknown counts as not
/// muted, like the `nirux` script.
fn sink_muted() -> bool {
    let Some(wpctl) = in_path("wpctl") else {
        return false;
    };
    Command::new(wpctl)
        .args(["get-volume", "@DEFAULT_AUDIO_SINK@"])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).contains("[MUTED]"))
        .unwrap_or(false)
}

/// The player command for FILE at VOLUME percent, without a shell.
fn player(file: &Path, volume: u8) -> Option<Command> {
    if let Some(pw) = in_path("pw-play") {
        let mut command = Command::new(pw);
        command
            .arg(format!("--volume={:.2}", f32::from(volume) / 100.0))
            .arg(file);
        return Some(command);
    }
    if let Some(pa) = in_path("paplay") {
        let mut command = Command::new(pa);
        command
            .arg(format!("--volume={}", u32::from(volume) * 65536 / 100))
            .arg(file);
        return Some(command);
    }
    if let Some(ff) = in_path("ffplay") {
        let mut command = Command::new(ff);
        command
            .args(["-nodisp", "-autoexit", "-loglevel", "quiet", "-volume"])
            .arg(volume.to_string())
            .arg(file);
        return Some(command);
    }
    None
}

/// The decision for one request, separate from playing so it can be tested.
pub struct Engine {
    pub paths: Paths,
    pub cached: Cached,
    limit: RateLimit,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Play { file: PathBuf, volume: u8 },
    Off,
    RateLimited,
    Muted,
    Missing,
}

impl Engine {
    pub fn new(paths: Paths) -> Self {
        let cached = Cached::new(paths.config.clone());
        Self {
            paths,
            cached,
            limit: RateLimit::default(),
        }
    }

    /// MUTED is asked only when the answer matters, so a disabled event or a
    /// rate-limited key repeat never starts `wpctl`.
    pub fn decide(
        &mut self,
        request: &Request,
        now: Instant,
        muted: &mut dyn FnMut() -> bool,
    ) -> Decision {
        let settings = self.cached.refresh().clone();
        if !settings.event_enabled(&request.event) {
            return Decision::Off;
        }
        if is_key_repeat(&request.event)
            && !self.limit.allow(&request.event, now, settings.rate_limit())
        {
            return Decision::RateLimited;
        }
        if settings.respect_mute() && request.muted.unwrap_or_else(&mut *muted) {
            return Decision::Muted;
        }
        match self.paths.file_for(&request.event, request.level) {
            Some(file) => Decision::Play {
                file,
                volume: settings.volume_for(&request.event),
            },
            None => Decision::Missing,
        }
    }
}

/// Start the worker that plays requests. It blocks while idle and wakes only
/// to reap players that are still running.
pub fn start() -> Sender<Request> {
    let (tx, rx) = mpsc::channel::<Request>();
    let _ = thread::Builder::new()
        .name("nirunotify-sounds".into())
        .spawn(move || worker(rx, Engine::new(Paths::from_env())));
    tx
}

fn worker(rx: Receiver<Request>, mut engine: Engine) {
    let mut children: Vec<Child> = Vec::new();
    loop {
        let request = if children.is_empty() {
            match rx.recv() {
                Ok(request) => Some(request),
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(Duration::from_millis(500)) {
                Ok(request) => Some(request),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => return,
            }
        };
        children.retain_mut(|child| !matches!(child.try_wait(), Ok(Some(_)) | Err(_)));
        let Some(request) = request else { continue };
        let start = Instant::now();
        let decision = engine.decide(&request, start, &mut sink_muted);
        if let Decision::Play { file, volume } = &decision {
            // A runaway burst can never pile up players.
            if children.len() >= 8 {
                continue;
            }
            if let Some(mut command) = player(file, *volume) {
                if let Ok(child) = command
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    children.push(child);
                }
            }
        }
        debug(&format!(
            "{} {:?}: {decision:?} in {} µs",
            request.event,
            request.level,
            start.elapsed().as_micros()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEFAULTS: &str = include_str!("../tests/fixtures/sounds-default.toml");

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("nirunotify-sounds-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn defaults_match_the_nirux_config() {
        let s = Settings::parse(DEFAULTS);
        for (event, _, default) in EVENTS {
            assert_eq!(s.event_enabled(event), *default, "{event}");
        }
        assert_eq!(s.volume_for("volume-up"), 35);
        assert_eq!(s.rate_limit(), Duration::from_millis(80));
        assert_eq!(s.debounce(), Duration::from_millis(1500));
        assert_eq!((s.battery_low(), s.battery_critical()), (20, 5));
        assert!(s.device_source("usb") && !s.device_source("bluetooth"));
    }

    #[test]
    fn an_empty_or_missing_file_uses_the_defaults() {
        let s = Settings::parse("");
        for (event, _, default) in EVENTS {
            assert_eq!(s.event_enabled(event), *default, "{event}");
        }
    }

    #[test]
    fn legacy_keys_in_sounds_still_count() {
        // A 0.12.218 file: only [sounds] and [sounds.apps].
        let s = Settings::parse(
            "[sounds]\nenabled = true\nvolume = 20\nstartup = false\n\n[sounds.apps]\nsignal = \"off\"\n",
        );
        assert!(!s.event_enabled("startup"));
        assert!(s.event_enabled("notification"));
        assert!(s.event_enabled("volume-up"));
        assert_eq!(s.volume_for("power-plug"), 20);
        // The category table wins over [sounds].
        let s = Settings::parse("[sounds]\nstartup = false\n[sounds.system]\nstartup = true\n");
        assert!(s.event_enabled("startup"));
    }

    #[test]
    fn categories_and_the_global_switch_gate_every_event() {
        let s = Settings::parse("[sounds.feedback]\nenabled = false\n");
        assert!(!s.event_enabled("volume-up"));
        assert!(s.event_enabled("power-plug"));
        let s = Settings::parse("[sounds]\nenabled = false\n");
        assert!(EVENTS.iter().all(|(e, _, _)| !s.event_enabled(e)));
        let s = Settings::parse("[sounds.feedback]\nbrightness-up = true\n");
        assert!(s.event_enabled("brightness-up"));
        assert!(!s.event_enabled("brightness-down"));
    }

    #[test]
    fn parsing_follows_the_nirux_awk_rules() {
        let s = Settings::parse(
            "volume = 50 # before any header\n[Sounds.Power]\n  \"Battery_Low_Percent\" = 30\nbattery-low = false  # comment\nbattery-low = true\n[sounds.power]\n",
        );
        assert_eq!(s.volume_for("startup"), 50);
        // Section names are not case-folded by nirux either.
        assert_eq!(s.battery_low(), 20);
        let s = Settings::parse("[sounds.power]\n\"Battery_Low_Percent\" = 30\nbattery-low = false  # c\nbattery-low = true\nvolume = \"70\"\n");
        assert_eq!(s.battery_low(), 30);
        assert!(!s.event_setting("battery-low"), "the first key wins");
        assert_eq!(s.volume_for("battery-low"), 70);
        assert_eq!(s.volume_for("auth-fail"), 35);
        let s = Settings::parse("[sounds]\nvolume = 900\n");
        assert_eq!(s.volume_for("mute"), 100);
    }

    #[test]
    fn requests_parse_from_the_socket() {
        assert_eq!(
            Request::parse("volume-up 45 unmuted"),
            Some(Request {
                event: "volume-up".into(),
                level: Some(45),
                muted: Some(false)
            })
        );
        assert_eq!(Request::parse("auth-fail"), Some(Request::new("auth-fail")));
        assert_eq!(Request::parse("nosuch"), None);
        assert_eq!(Request::parse("volume-up loud"), None);
        assert_eq!(Request::parse("volume-down 30%").unwrap().level, Some(30));
    }

    #[test]
    fn the_rate_limit_drops_held_key_repeats() {
        let mut limit = RateLimit::default();
        let t = Instant::now();
        let w = Duration::from_millis(80);
        assert!(limit.allow("volume-up", t, w));
        assert!(!limit.allow("volume-up", t + Duration::from_millis(30), w));
        assert!(limit.allow("volume-down", t + Duration::from_millis(30), w));
        assert!(limit.allow("volume-up", t + Duration::from_millis(81), w));
    }

    #[test]
    fn files_resolve_with_overrides_and_levels() {
        let root = temp_dir("files");
        let paths = Paths {
            config: root.join("sounds.toml"),
            user: root.join("user"),
            nirux: root.join("nirux"),
        };
        std::fs::create_dir_all(paths.nirux.join("levels")).unwrap();
        std::fs::create_dir_all(&paths.user).unwrap();
        for f in [
            "startup",
            "volume-up",
            "levels/volume-up-050",
            "levels/volume-up-100",
        ] {
            std::fs::write(paths.nirux.join(format!("{f}.ogg")), b"OggS").unwrap();
        }
        assert_eq!(
            paths.file_for("volume-up", Some(47)),
            Some(paths.nirux.join("levels/volume-up-050.ogg"))
        );
        assert_eq!(
            paths.file_for("volume-up", Some(250)),
            Some(paths.nirux.join("levels/volume-up-100.ogg"))
        );
        assert_eq!(
            paths.file_for("volume-up", Some(10)),
            Some(paths.nirux.join("volume-up.ogg"))
        );
        assert_eq!(
            paths.file_for("volume-up", None),
            Some(paths.nirux.join("volume-up.ogg"))
        );
        assert_eq!(paths.file_for("lock", None), None);
        std::fs::write(paths.user.join("startup.ogg"), b"OggS").unwrap();
        std::fs::write(paths.user.join("volume-up.ogg"), b"OggS").unwrap();
        assert_eq!(
            paths.file_for("startup", None),
            Some(paths.user.join("startup.ogg"))
        );
        assert_eq!(
            paths.file_for("volume-up", Some(50)),
            Some(paths.user.join("volume-up.ogg"))
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_engine_checks_switches_limit_and_mute_in_order() {
        let root = temp_dir("engine");
        let paths = Paths {
            config: root.join("sounds.toml"),
            user: root.join("user"),
            nirux: root.join("nirux"),
        };
        std::fs::create_dir_all(&paths.nirux).unwrap();
        for f in ["volume-up", "power-plug", "unmute"] {
            std::fs::write(paths.nirux.join(format!("{f}.ogg")), b"OggS").unwrap();
        }
        std::fs::write(&paths.config, DEFAULTS).unwrap();
        let mut engine = Engine::new(paths.clone());
        let t = Instant::now();
        let mut asked = 0;
        let mut unmuted = || false;
        let d = engine.decide(
            &Request::parse("volume-up 50 unmuted").unwrap(),
            t,
            &mut unmuted,
        );
        assert_eq!(
            d,
            Decision::Play {
                file: paths.nirux.join("volume-up.ogg"),
                volume: 35
            }
        );
        let d = engine.decide(
            &Request::parse("volume-up 55 unmuted").unwrap(),
            t + Duration::from_millis(20),
            &mut unmuted,
        );
        assert_eq!(d, Decision::RateLimited);
        let mut counting = || {
            asked += 1;
            true
        };
        let d = engine.decide(&Request::new("brightness-up"), t, &mut counting);
        assert_eq!(d, Decision::Off);
        let d = engine.decide(&Request::new("power-plug"), t, &mut counting);
        assert_eq!(d, Decision::Muted);
        assert_eq!(asked, 1, "mute is asked only when it matters");
        let mut is_muted = || true;
        let d = engine.decide(&Request::parse("unmute unmuted").unwrap(), t, &mut is_muted);
        assert!(
            matches!(d, Decision::Play { .. }),
            "the caller's state wins: {d:?}"
        );
        assert_eq!(
            engine.decide(&Request::new("battery-low"), t, &mut unmuted),
            Decision::Missing
        );
        // A change on disk is picked up without a restart.
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(
            &paths.config,
            "[sounds.power]\nenabled = false\nvolume = 60\n",
        )
        .unwrap();
        assert_eq!(
            engine.decide(&Request::new("power-plug"), t, &mut unmuted),
            Decision::Off
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
