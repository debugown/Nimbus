//! The app's own settings, one small JSON file in `%APPDATA%\Nimbus Native\settings.json`.
//! Each part of the app keeps its settings under its own key, so they don't overwrite each other.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

pub fn file() -> Option<PathBuf> {
    Some(
        PathBuf::from(std::env::var_os("APPDATA")?)
            .join("Nimbus Native")
            .join("settings.json"),
    )
}

/// Everything saved; an empty object when there's nothing or it can't be read.
pub fn read_from(path: &Path) -> Value {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({}))
}

/// Saves one key, leaving the others as they are.
pub fn write_key(path: &Path, key: &str, value: Value) {
    let mut all = read_from(path);
    all[key] = value;
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, all.to_string());
}

pub fn read() -> Value {
    file().map(|f| read_from(&f)).unwrap_or_else(|| json!({}))
}

pub fn write(key: &str, value: Value) {
    if let Some(f) = file() {
        write_key(&f, key, value);
    }
}

/// How a screen share starts: height (0 = the source's own), frame rate, with sound, and where
/// it goes (0 the Nimbus room, 1 Discord, 2 both).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Share {
    pub height: i32,
    pub fps: i32,
    pub sound: bool,
    pub dest: i32,
}

impl Share {
    pub const DEFAULT: Share = Share {
        height: 0,
        fps: 60,
        sound: true,
        dest: 1,
    };

    pub fn from_json(v: &Value) -> Self {
        let d = Self::DEFAULT;
        let height = v["height"]
            .as_i64()
            .map(|h| h as i32)
            .filter(|h| matches!(h, 0 | 720 | 1080 | 1440))
            .unwrap_or(d.height);
        let fps = v["fps"]
            .as_i64()
            .map(|f| f as i32)
            .filter(|f| matches!(f, 15 | 30 | 60))
            .unwrap_or(d.fps);
        Self {
            height,
            fps,
            sound: v["sound"].as_bool().unwrap_or(d.sound),
            dest: v["dest"].as_i64().filter(|d| matches!(d, 0..=2)).map(|d| d as i32).unwrap_or(d.dest),
        }
    }

    pub fn to_json(self) -> Value {
        json!({ "height": self.height, "fps": self.fps, "sound": self.sound, "dest": self.dest })
    }
}

/// Looks and reading settings (Внешний вид, Специальные возможности, Язык и время).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Look {
    pub scale: f32,
    pub compact: bool,
    pub show_time: bool,
    pub reduce_motion: bool,
    /// How a new message comes in: 0 from the side, 1 side then typing, 2 fade and zoom.
    pub message_anim: i32,
    pub contrast: bool,
    pub time12: bool,
    pub date_iso: bool,
    /// 0 black, 1 graphite, 2 midnight, 3 light.
    pub theme: i32,
    pub glass: bool,
    /// 0 violet, 1 blue, 2 green, 3 pink, 4 orange.
    pub accent: i32,
    pub streamer: bool,
    /// Streamer mode switches on by itself while a screen is shown.
    pub streamer_auto: bool,
    pub hide_from_capture: bool,
    /// The colour of the app icon: 0 as made, 1..3 others.
    pub app_icon: i32,
}

impl Look {
    pub const DEFAULT: Look = Look {
        scale: 1.0,
        compact: false,
        show_time: true,
        reduce_motion: false,
        message_anim: 2,
        contrast: false,
        time12: false,
        date_iso: false,
        theme: 0,
        glass: true,
        accent: 0,
        streamer: false,
        streamer_auto: true,
        hide_from_capture: false,
        app_icon: 0,
    };

    pub fn from_json(v: &Value) -> Self {
        let d = Self::DEFAULT;
        let flag = |k: &str, def: bool| v[k].as_bool().unwrap_or(def);
        Self {
            scale: v["scale"].as_f64().map(|s| (s as f32).clamp(0.8, 1.5)).unwrap_or(d.scale),
            compact: flag("compact", d.compact),
            show_time: flag("show_time", d.show_time),
            reduce_motion: flag("reduce_motion", d.reduce_motion),
            message_anim: v["message_anim"].as_i64().map(|n| (n as i32).clamp(0, 2)).unwrap_or(d.message_anim),
            contrast: flag("contrast", d.contrast),
            time12: flag("time12", d.time12),
            date_iso: flag("date_iso", d.date_iso),
            glass: v["glass"].as_bool().unwrap_or(d.glass),
            theme: v["theme"].as_i64().map(|n| (n as i32).clamp(0, 3)).unwrap_or(d.theme),
            accent: v["accent"].as_i64().map(|n| (n as i32).clamp(0, 4)).unwrap_or(d.accent),
            streamer: flag("streamer", d.streamer),
            streamer_auto: flag("streamer_auto", d.streamer_auto),
            hide_from_capture: flag("hide_from_capture", d.hide_from_capture),
            app_icon: v["app_icon"].as_i64().map(|n| (n as i32).clamp(0, 3)).unwrap_or(d.app_icon),
        }
    }

    pub fn to_json(self) -> Value {
        json!({
            "scale": self.scale, "compact": self.compact, "show_time": self.show_time, "reduce_motion": self.reduce_motion, "message_anim": self.message_anim,
            "contrast": self.contrast, "time12": self.time12, "date_iso": self.date_iso,
            "theme": self.theme, "glass": self.glass, "accent": self.accent, "streamer": self.streamer, "streamer_auto": self.streamer_auto, "hide_from_capture": self.hide_from_capture, "app_icon": self.app_icon,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn look_round_trips_and_clamps() {
        let look = Look {
            scale: 1.15,
            compact: true,
            show_time: false,
            contrast: true,
            time12: true,
            theme: 2,
            accent: 3,
            streamer: true,
            app_icon: 1,
            message_anim: 1,
            ..Look::DEFAULT
        };
        assert_eq!(Look::from_json(&look.to_json()), look);
        assert_eq!(Look::from_json(&json!({ "scale": 9.0 })).scale, 1.5);
        assert_eq!(Look::from_json(&json!({ "message_anim": 9 })).message_anim, 2);
        assert_eq!(Look::from_json(&json!({ "theme": 9, "accent": -3, "app_icon": 7 })).theme, 3);
        assert_eq!(Look::from_json(&json!({ "accent": -3 })).accent, 0);
        assert_eq!(Look::from_json(&json!({ "app_icon": 7 })).app_icon, 3);
        assert_eq!(Look::from_json(&json!(null)), Look::DEFAULT);
    }

    #[test]
    fn keys_do_not_overwrite_each_other() {
        let dir = std::env::temp_dir().join(format!("nimbus-prefs-test-{}", std::process::id()));
        let file = dir.join("settings.json");
        write_key(
            &file,
            "share",
            Share {
                height: 1080,
                fps: 30,
                sound: false,
                dest: 2,
            }
            .to_json(),
        );
        write_key(&file, "other", json!({ "a": 1 }));
        let all = read_from(&file);
        assert_eq!(
            Share::from_json(&all["share"]),
            Share {
                height: 1080,
                fps: 30,
                sound: false,
                dest: 2
            }
        );
        assert_eq!(all["other"]["a"], 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn bad_values_fall_back_to_the_defaults() {
        assert_eq!(
            Share::from_json(&json!({ "height": 999, "fps": 7, "sound": "yes", "dest": 5 })),
            Share::DEFAULT
        );
        assert_eq!(Share::from_json(&json!(null)), Share::DEFAULT);
        assert_eq!(read_from(Path::new("Z:\\no\\such\\file.json")), json!({}));
    }
}
