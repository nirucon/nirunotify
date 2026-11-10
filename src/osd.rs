//! Pure parts of the hardware OSD: what a message means and how it looks.
//!
//! The X11 side lives in `main.rs`; everything here is plain data so it can
//! be tested without a display.

use std::time::Duration;

/// How long the bar takes to glide to a new level.
pub const GLIDE: Duration = Duration::from_millis(140);
/// How long the bar lights up when a key presses against 0 % or 100 %.
pub const FLASH: Duration = Duration::from_millis(180);
/// How long the OSD stays after the last key press.
pub const LINGER: Duration = Duration::from_millis(1300);

/// What the text of `osd CHANNEL TEXT` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reading {
    /// `45%` or a bare `45` (0..=100).
    Level(u32),
    /// `Muted`: the channel keeps its last level, drawn muted.
    Muted,
    /// Anything else is shown as text without a bar.
    Text(String),
}

pub fn read(text: &str) -> Reading {
    let t = text.trim();
    if t.eq_ignore_ascii_case("muted") || t.eq_ignore_ascii_case("mute") {
        return Reading::Muted;
    }
    let digits = t.strip_suffix('%').unwrap_or(t).trim();
    match digits.parse::<u32>() {
        Ok(n) if !digits.is_empty() => Reading::Level(n.min(100)),
        _ => Reading::Text(t.to_string()),
    }
}

/// Human label for a channel name such as `volume` or `mic-volume`.
pub fn label(channel: &str) -> String {
    match channel {
        "volume" => "Volume".into(),
        "brightness" => "Brightness".into(),
        "mic" | "microphone" => "Microphone".into(),
        "keyboard-brightness" | "kbd-brightness" => "Keyboard light".into(),
        other => {
            let words = other.replace(['-', '_'], " ");
            let mut chars = words.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect(),
                None => String::new(),
            }
        }
    }
}

/// Nerd Font icon candidates for a channel, used only when the font has it.
pub fn icon(channel: &str, level: Option<u32>, muted: bool) -> Option<char> {
    match channel {
        "volume" if muted => Some('\u{f075f}'),
        "volume" => Some(match level.unwrap_or(50) {
            0 => '\u{f0581}',
            1..=33 => '\u{f057f}',
            34..=66 => '\u{f0580}',
            _ => '\u{f057e}',
        }),
        "brightness" => Some(match level.unwrap_or(50) {
            0..=33 => '\u{f00de}',
            34..=66 => '\u{f00df}',
            _ => '\u{f00e0}',
        }),
        "mic" | "microphone" if muted => Some('\u{f036d}'),
        "mic" | "microphone" => Some('\u{f036c}'),
        "keyboard-brightness" | "kbd-brightness" => Some('\u{f030c}'),
        _ => None,
    }
}

/// Ease-out cubic between `from` and `to`, `t` in 0..=1.
pub fn glide(from: f32, to: f32, t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let k = 1.0 - (1.0 - t).powi(3);
    from + (to - from) * k
}

/// Filled width of a bar `track` pixels wide at `level` percent.
pub fn fill_width(track: u16, level: f32) -> u16 {
    ((f32::from(track) * level.clamp(0.0, 100.0) / 100.0).round() as u16).min(track)
}

/// Sizes of the OSD box, scaled from the notification font size.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    pub width: u16,
    pub padding: i16,
    pub small: f32,
    pub big: f32,
    pub gap: i16,
    pub bar: u16,
}

pub fn metrics(font_size: f32) -> Metrics {
    let scale = (font_size / 14.0).clamp(0.6, 2.4);
    Metrics {
        width: ((300.0 * scale).round() as u16).clamp(220, 640),
        padding: (16.0 * scale).round() as i16,
        small: font_size,
        big: (font_size * 1.9).round(),
        gap: (12.0 * scale).round() as i16,
        bar: ((6.0 * scale).round() as u16).max(4),
    }
}

/// Vertical position of the box on a monitor: in the lower part of the
/// usable area, clear of the bar, where it does not cover what is being read.
pub fn top(origin_y: i16, area_h: u16, reserved_top: u16, reserved_bottom: u16, h: u16) -> i16 {
    let usable = i32::from(area_h) - i32::from(reserved_top) - i32::from(reserved_bottom);
    let lift = (usable / 8).max(24);
    let y = i32::from(origin_y) + i32::from(reserved_top) + usable - lift - i32::from(h);
    let min = i32::from(origin_y) + i32::from(reserved_top);
    y.max(min).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_levels_mute_and_text() {
        assert_eq!(read("45%"), Reading::Level(45));
        assert_eq!(read(" 7 "), Reading::Level(7));
        assert_eq!(read("140%"), Reading::Level(100));
        assert_eq!(read("Muted"), Reading::Muted);
        assert_eq!(read("--%"), Reading::Text("--%".into()));
        assert_eq!(
            read("Night light on"),
            Reading::Text("Night light on".into())
        );
        assert_eq!(read("%"), Reading::Text("%".into()));
    }

    #[test]
    fn labels_channels() {
        assert_eq!(label("volume"), "Volume");
        assert_eq!(label("brightness"), "Brightness");
        assert_eq!(label("night-light"), "Night light");
        assert_eq!(label(""), "");
    }

    #[test]
    fn icons_follow_level_and_mute() {
        assert_eq!(icon("volume", Some(90), false), Some('\u{f057e}'));
        assert_eq!(icon("volume", Some(0), false), Some('\u{f0581}'));
        assert_eq!(icon("volume", Some(90), true), Some('\u{f075f}'));
        assert_eq!(icon("other", None, false), None);
    }

    #[test]
    fn glide_eases_and_ends_on_target() {
        assert_eq!(glide(10.0, 60.0, 0.0), 10.0);
        assert_eq!(glide(10.0, 60.0, 1.0), 60.0);
        let mid = glide(10.0, 60.0, 0.5);
        assert!(mid > 35.0 && mid < 60.0, "{mid}");
        assert_eq!(glide(10.0, 60.0, 4.0), 60.0);
    }

    #[test]
    fn fill_is_bounded() {
        assert_eq!(fill_width(200, 0.0), 0);
        assert_eq!(fill_width(200, 50.0), 100);
        assert_eq!(fill_width(200, 100.0), 200);
        assert_eq!(fill_width(200, 180.0), 200);
    }

    #[test]
    fn box_sits_low_on_its_own_monitor() {
        // Lower of two stacked 1440p screens, 28 px bar at the top.
        let y = top(1440, 1440, 28, 0, 90);
        assert!(y > 1440 + 1440 / 2 && y + 90 < 2880, "{y}");
        // A tiny area never pushes it above the monitor.
        assert_eq!(top(0, 100, 28, 0, 90), 28);
    }

    #[test]
    fn metrics_scale_with_font() {
        let m = metrics(14.0);
        assert_eq!(m.width, 300);
        assert!(metrics(20.0).width > m.width);
        assert!(m.big > m.small);
    }
}
