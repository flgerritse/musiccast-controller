//! Device model and persistence.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BandRange {
    pub min: i64,
    pub max: i64,
    pub step: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Preset {
    pub num: i64,
    pub band: String,
    pub text: String,
}

impl Preset {
    /// Label format `<n>. <band> <number> - <text>`;
    /// text "unknown" means an empty slot -> `<n>. - `.
    pub fn label(&self, index: usize) -> String {
        // Blank or "unknown" text (or band) is an empty slot; trim so spaces don't count.
        let text = self.text.trim();
        let band = self.band.trim();
        if text.is_empty() || text.eq_ignore_ascii_case("unknown") || band.eq_ignore_ascii_case("unknown") {
            format!("{}. - ", index + 1)
        } else if band.is_empty() {
            format!("{}. {}", index + 1, text)
        } else {
            format!("{}. {} {} - {}", index + 1, band, self.num, text)
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InputItem {
    pub id: String,
    pub title: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tone {
    pub name: String,
    pub value: i64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Device {
    pub ip: String,
    pub base_url: String,
    pub name: String,
    pub model: String,
    pub serial: String,
    pub icon_url: String,
    /// AVTransport control URL for DLNA push (MusicCast uses /AVTransport/ctrl).
    pub dlna_url: String,
    /// Input id used for DLNA push ("server" terminal in system.input_list).
    pub dlna_input: String,
    pub is_manual: bool,
    pub alive: bool,

    // main/getStatus
    pub power_on: bool,
    pub volume: i64,
    pub volume_max: i64,
    pub volume_min: i64,
    pub volume_step: i64,
    pub muted: bool,
    pub input: String,
    pub sound_program: String,
    pub surr_decode: String,
    pub direct: bool,
    pub enhancer: bool,
    pub bass_extension: bool,
    pub clear_voice: bool,
    pub party: bool,
    pub contents_display: bool,

    // netusb/getPlayInfo
    pub playback: String,
    pub playing: bool,
    pub stopped: bool,
    pub repeat: String,
    pub shuffle: bool,
    pub repeat_available: bool,
    pub shuffle_available: bool,
    pub track: String,
    pub artist: String,
    pub album: String,
    pub art_url: String,
    pub play_time: i64,
    pub total_time: i64,

    // system/getFeatures
    pub inputs: Vec<InputItem>,
    pub zones: Vec<String>,
    pub zone: String,
    pub func_list: Vec<String>,
    pub sys_func_list: Vec<String>,
    pub programs: Vec<String>,
    pub dimmer: i64,
    pub dim_min: i64,
    pub dim_max: i64,
    pub dim_step: i64,
    pub tone_mode: String,
    pub tone_type: String,
    pub tone_min: i64,
    pub tone_max: i64,
    pub tone_step: i64,
    pub tones: Vec<Tone>,
    pub bands: Vec<String>,
    pub band: String,
    pub band_ranges: BTreeMap<String, BandRange>,
    pub tuner_freq: i64,
    pub presets: Vec<Preset>,
    pub tuner_presets: Vec<Preset>,

    // main/getSignalInfo
    pub au_format: String,
    pub au_fs: String,
}

impl Device {
    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
    pub fn has_tuner(&self) -> bool {
        !self.bands.is_empty()
    }
    pub fn current_band_range(&self) -> BandRange {
        if let Some(r) = self.band_ranges.get(&self.band) {
            return r.clone();
        }
        // Fallback ranges (estimated; devices normally report tuner range_step).
        match self.band.to_lowercase().as_str() {
            "am" => BandRange { min: 531, max: 1611, step: 9 },
            _ => BandRange { min: 87500, max: 108000, step: 100 },
        }
    }
    pub fn display_name(&self) -> String {
        if self.name.is_empty() {
            self.ip.clone()
        } else {
            self.name.clone()
        }
    }
}

/// One entry of a netusb list browse (getListInfo).
#[derive(Clone, Debug, Default)]
pub struct ListEntry {
    pub text: String,
    pub attribute: i64,
}

/// Play-history record, kept in a JSON store.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HistoryRow {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: i64,
    pub mediauri: String,
    pub play_count: i64,
    pub last_update: String,
    pub speaker_name: String,
    pub speaker_ip: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    #[default]
    English,
    Dutch,
}

impl Language {
    pub fn text<'a>(self, english: &'a str, dutch: &'a str) -> &'a str {
        match self { Self::English => english, Self::Dutch => dutch }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Appearance {
    #[default]
    System,
    Light,
    Dark,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub manual_ips: Vec<String>,
    pub update_seconds: u32,
    pub language: Language,
    pub appearance: Appearance,
}

impl Default for Config {
    fn default() -> Self {
        Self { manual_ips: Vec::new(), update_seconds: 8, language: Language::English, appearance: Appearance::System }
    }
}

impl Config {
    pub fn load() -> Config {
        // Migrate existing installations' settings on the next save.
        [config_path(), data_dir().join("speaker-control-config.json")]
            .iter()
            .find_map(|p| std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()))
            .unwrap_or_default()
    }
    pub fn save(&self) -> std::io::Result<()> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let data = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, data)
    }
}

/// Runtime files live next to the executable (inside app/).
pub fn data_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn config_path() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    base.map(|p| p.join("Controller for MusicCast").join("settings.json"))
        .unwrap_or_else(|| data_dir().join("speaker-control-config.json"))
}

pub fn history_path() -> PathBuf {
    data_dir().join("music-history.json")
}

pub fn load_history() -> Vec<HistoryRow> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Mirrors DBHelper save:song:media:player: — match on title+artist+album+duration,
/// increment playCount, else insert.
pub fn record_play(row: HistoryRow) {
    let mut hist = load_history();
    if let Some(existing) = hist.iter_mut().find(|h| {
        h.title == row.title && h.artist == row.artist && h.album == row.album && h.duration == row.duration
    }) {
        existing.play_count += 1;
        existing.mediauri = row.mediauri;
        existing.last_update = row.last_update;
    } else {
        hist.push(row);
    }
    if let Ok(s) = serde_json::to_string_pretty(&hist) {
        let _ = std::fs::write(history_path(), s);
    }
}

/// Round a value to the NEAREST multiple of step, ties going down
/// (the remainder is compared with step>>1 and one step is added only when it
/// is greater).
/// Steps below 2 leave the value unchanged.
pub fn step_val(v: i64, step: i64) -> i64 {
    if step >= 2 {
        let q = (v / step) * step;
        let r = v - q;
        q + if r > step / 2 { step } else { 0 }
    } else {
        v
    }
}

pub fn fmt_time(secs: i64) -> String {
    let s = secs.max(0);
    format!("{}:{:02}", s / 60, s % 60)
}

pub fn now_string() -> String {
    // Simple UTC timestamp without extra deps.
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_secs())
}