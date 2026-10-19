//! NIRUNOTIFY configuration and suite theme colours.

use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Position {
    TopRight,
    TopLeft,
    BottomRight,
    BottomLeft,
}

#[derive(Clone, Debug)]
pub struct Config {
    pub width: u16,
    pub margin: i16,
    pub max_visible: usize,
    pub default_timeout_ms: u64,
    pub max_timeout_ms: u64,
    pub font_path: Option<String>,
    pub font_size: f32,
    pub position: Position,
    pub palette: Palette,
}

#[derive(Clone, Copy, Debug)]
pub struct Palette {
    pub background: u32,
    pub surface: u32,
    pub foreground: u32,
    pub muted: u32,
    pub border: u32,
    pub accent: u32,
    pub warning: u32,
    pub error: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            background: 0x0f0f10,
            surface: 0x161618,
            foreground: 0xe5e5e5,
            muted: 0xa8a8a8,
            border: 0x2a2a2d,
            accent: 0x5a5a60,
            warning: 0xd7b36a,
            error: 0xd77a7a,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            width: 360,
            margin: 12,
            max_visible: 5,
            default_timeout_ms: 6000,
            max_timeout_ms: 60_000,
            font_path: None,
            font_size: 14.0,
            position: Position::TopRight,
            palette: Palette::default(),
        }
    }
}

fn config_home() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".config")
        })
}

pub fn path() -> PathBuf {
    config_home().join("nirunotify/config.toml")
}

fn parse_hex(s: &str) -> Option<u32> {
    let s = s.trim();
    (s.len() == 6 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| u32::from_str_radix(s, 16).ok())
        .flatten()
}

fn builtin_theme(name: &str) -> Option<Palette> {
    let (background, foreground, muted, border, focus) = match name {
        "niru-noir" => (0x0f0f10, 0xe5e5e5, 0xa8a8a8, 0x2a2a2d, 0x5a5a60),
        "niru-nord" => (0x2e3440, 0xeceff4, 0x9aa5b1, 0x3b4252, 0x88c0d0),
        "niru-satie" => (0x141414, 0xdedad4, 0x8f8b86, 0x2a2724, 0xb08d7d),
        "niru-carl-larsson" => (0xf3ece0, 0x3b3228, 0x7d6f5f, 0xd8cdbb, 0xa63d2f),
        _ => return None,
    };
    Some(Palette {
        background,
        surface: blend(background, foreground, 0.08),
        foreground,
        muted,
        border,
        accent: focus,
        warning: 0xd7b36a,
        error: 0xd77a7a,
    })
}

fn blend(a: u32, b: u32, amount: f32) -> u32 {
    let mix = |x: u32, y: u32| {
        let x = (x & 0xff) as f32;
        let y = (y & 0xff) as f32;
        (x + (y - x) * amount).round() as u32
    };
    (mix(a >> 16, b >> 16) << 16)
        | (mix((a >> 8) & 0xff, (b >> 8) & 0xff) << 8)
        | mix(a & 0xff, b & 0xff)
}

fn suite_palette_from(base: &std::path::Path) -> Palette {
    let Ok(config) = std::fs::read_to_string(base.join("niruwm/config.toml")) else {
        return Palette::default();
    };
    let Ok(value) = config.parse::<toml::Value>() else {
        return Palette::default();
    };
    let name = value
        .get("theme")
        .and_then(|v| v.as_str())
        .unwrap_or("niru-noir");
    let mut palette = builtin_theme(name).unwrap_or_default();
    let Ok(text) = std::fs::read_to_string(base.join("niruwm/themes").join(format!("{name}.toml")))
    else {
        return palette;
    };
    let Ok(theme) = text.parse::<toml::Value>() else {
        return palette;
    };
    let get = |field: &str, fallback: u32| {
        theme
            .get(field)
            .and_then(|v| v.as_str())
            .and_then(parse_hex)
            .unwrap_or(fallback)
    };
    palette.background = get("background", palette.background);
    palette.surface = get("surface", palette.surface);
    palette.foreground = get("foreground", palette.foreground);
    palette.muted = get("muted", palette.muted);
    palette.border = get("border", palette.border);
    palette.accent = get("accent", get("focus", palette.accent));
    palette.warning = get("warning", palette.warning);
    palette.error = get("error", palette.error);
    palette
}

impl Config {
    pub fn load() -> Result<Self> {
        let path = path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(format!("{}: {e}", path.display()).into()),
        };
        let value: toml::Value = text
            .parse()
            .map_err(|e| format!("{}: {e}", path.display()))?;
        Self::from_value(&value).map_err(|e| format!("{}: {e}", path.display()).into())
    }

    pub fn from_value(v: &toml::Value) -> Result<Self> {
        let mut cfg = Self::default();
        if let Some(n) = v.get("width").and_then(|x| x.as_integer()) {
            cfg.width = n.clamp(220, 900) as u16;
        }
        if let Some(n) = v.get("margin").and_then(|x| x.as_integer()) {
            cfg.margin = n.clamp(0, 100) as i16;
        }
        if let Some(n) = v.get("max_visible").and_then(|x| x.as_integer()) {
            cfg.max_visible = n.clamp(1, 12) as usize;
        }
        if let Some(n) = v.get("default_timeout_ms").and_then(|x| x.as_integer()) {
            cfg.default_timeout_ms = n.clamp(500, 600_000) as u64;
        }
        if let Some(n) = v.get("max_timeout_ms").and_then(|x| x.as_integer()) {
            cfg.max_timeout_ms = n.clamp(1_000, 600_000) as u64;
        }
        if let Some(n) = v.get("font_size").and_then(|x| x.as_float()) {
            cfg.font_size = n as f32;
        }
        if let Some(s) = v.get("font_path").and_then(|x| x.as_str()) {
            cfg.font_path = Some(s.to_string());
        }
        if let Some(s) = v.get("position").and_then(|x| x.as_str()) {
            cfg.position = match s {
                "top-right" => Position::TopRight,
                "top-left" => Position::TopLeft,
                "bottom-right" => Position::BottomRight,
                "bottom-left" => Position::BottomLeft,
                _ => {
                    return Err(
                        "position must be top-right, top-left, bottom-right or bottom-left".into(),
                    )
                }
            };
        }
        cfg.palette = suite_palette_from(&config_home());
        if !(8.0..=32.0).contains(&cfg.font_size) {
            return Err("font_size must be 8..32".into());
        }
        Ok(cfg)
    }

    pub fn describe(&self) -> String {
        format!(
            "config: {}\nwidth: {}\nmargin: {}\nmax visible: {}\ntimeouts: {}..{} ms\nfont: {} {}\nposition: {:?}",
            path().display(),
            self.width,
            self.margin,
            self.max_visible,
            self.default_timeout_ms,
            self.max_timeout_ms,
            self.font_size,
            self.font_path.as_deref().unwrap_or("auto"),
            self.position
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounds_are_clamped() {
        let v: toml::Value = "width = 10\nmax_visible = 100".parse().unwrap();
        let c = Config::from_value(&v).unwrap();
        assert_eq!(c.width, 220);
        assert_eq!(c.max_visible, 12);
    }

    #[test]
    fn position_is_validated() {
        let v: toml::Value = "position = \"sideways\"".parse().unwrap();
        assert!(Config::from_value(&v).is_err());
    }
}
