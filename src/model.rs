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
    /// Blank or "unknown" text (or band) is an empty slot; trim so spaces don't count.
    pub fn is_empty(&self) -> bool {
        let text = self.text.trim();
        text.is_empty() || text.eq_ignore_ascii_case("unknown") || self.band.trim().eq_ignore_ascii_case("unknown")
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct InputItem {
    pub id: String,
    pub title: String,
    /// "netusb", "tuner" or "none" (system.input_list play_info_type).
    pub play_info_type: String,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Tone {
    pub name: String,
    pub value: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ToneKind {
    #[default]
    ToneControl,
    Equalizer,
}

#[derive(Clone, Debug, Default)]
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
    /// True once getFeatures has been read for this device.
    pub initialised: bool,

    // <zone>/getStatus
    pub power_on: bool,
    pub volume: i64,
    pub volume_max: i64,
    pub volume_min: i64,
    pub volume_step: i64,
    /// actual_volume (e.g. -35.5 dB) when the device reports it.
    pub volume_db: Option<(f64, String)>,
    pub muted: bool,
    pub input: String,
    pub sound_program: String,
    pub direct: bool,
    pub enhancer: bool,
    pub bass_extension: bool,
    pub clear_voice: bool,

    // netusb/getPlayInfo
    pub playing: bool,
    pub paused: bool,
    pub stopped: bool,
    pub repeat: String,
    pub shuffle: String,
    pub repeat_modes: Vec<String>,
    pub shuffle_modes: Vec<String>,
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
    pub tone_kind: ToneKind,
    pub tone_min: i64,
    pub tone_max: i64,
    pub tone_step: i64,
    pub tones: Vec<Tone>,
    pub bands: Vec<String>,
    pub band: String,
    pub band_ranges: BTreeMap<String, BandRange>,
    /// tuner.preset.type: "common" (one list for all bands) or "separate".
    pub tuner_preset_type: String,
    pub tuner_freq: i64,
    pub tuner_station: String,
    pub tuner_text: String,
    pub presets: Vec<Preset>,
    pub tuner_presets: Vec<Preset>,

    // <zone>/getSignalInfo
    pub au_format: String,
    pub au_fs: String,

    // dist/getDistributionInfo (MusicCast Link)
    /// "server", "client" or "none".
    pub link_role: String,
    pub link_group_id: String,
    pub link_group_name: String,
    /// IP addresses of the clients (only filled on a server).
    pub link_clients: Vec<String>,
}

impl Device {
    pub fn api(&self, path: &str) -> String {
        format!("{}{}", self.base_url, path)
    }
    /// Zone-relative API path, e.g. `zpath("setVolume?volume=3")` -> `/zone2/setVolume?volume=3`.
    pub fn zpath(&self, cmd: &str) -> String {
        let zone = if self.zone.is_empty() { "main" } else { &self.zone };
        format!("/{zone}/{cmd}")
    }
    pub fn zone_or_main(&self) -> &str {
        if self.zone.is_empty() { "main" } else { &self.zone }
    }
    pub fn has_tuner(&self) -> bool {
        !self.bands.is_empty()
    }
    pub fn on_tuner(&self) -> bool {
        self.input == "tuner" || self.input_type() == "tuner"
    }
    /// play_info_type of the current input; unknown inputs are treated as netusb
    /// when the device does not report the type (older firmware).
    pub fn input_type(&self) -> &str {
        match self.inputs.iter().find(|i| i.id == self.input) {
            Some(i) if !i.play_info_type.is_empty() => &i.play_info_type,
            Some(_) | None => {
                if self.input == "tuner" {
                    "tuner"
                } else if is_external_input(&self.input) {
                    "none"
                } else {
                    "netusb"
                }
            }
        }
    }
    pub fn on_netusb(&self) -> bool {
        self.input_type() == "netusb"
    }
    pub fn on_dlna(&self) -> bool {
        !self.dlna_input.is_empty() && self.input == self.dlna_input
    }
    pub fn tuner_presets_common(&self) -> bool {
        self.tuner_preset_type == "common"
    }
    /// Range of the current tuner band; `None` when the band has no tunable
    /// frequency (DAB) or nothing is known about it.
    pub fn current_band_range(&self) -> Option<BandRange> {
        let band = self.band.to_lowercase();
        if band == "dab" {
            return None;
        }
        if let Some(r) = self.band_ranges.get(&self.band).filter(|r| r.max > r.min) {
            return Some(r.clone());
        }
        // Fallback ranges (estimated; devices normally report tuner range_step).
        match band.as_str() {
            "am" => Some(BandRange { min: 531, max: 1611, step: 9 }),
            "fm" => Some(BandRange { min: 87500, max: 108000, step: 50 }),
            _ => None,
        }
    }
    pub fn display_name(&self) -> String {
        if self.name.is_empty() { self.ip.clone() } else { self.name.clone() }
    }
    /// Server of a MusicCast Link group that has at least one client.
    pub fn is_link_server(&self) -> bool {
        self.link_role == "server" && !self.link_clients.is_empty() && valid_group_id(&self.link_group_id)
    }
    pub fn is_link_client(&self) -> bool {
        self.link_role == "client" && valid_group_id(&self.link_group_id)
    }
    pub fn input_title(&self, id: &str) -> String {
        self.inputs
            .iter()
            .find(|i| i.id == id)
            .map(|i| i.title.clone())
            .unwrap_or_else(|| input_display_name(id))
    }
}

/// The device reports an all-zero id when it is not in a group.
pub fn valid_group_id(id: &str) -> bool {
    !id.is_empty() && id.chars().any(|c| c != '0')
}

/// Random 32-hex-digit group id, as MusicCast Link expects.
pub fn new_group_id() -> String {
    use std::hash::{BuildHasher, Hasher};
    let part = |salt: u64| {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(salt);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos());
        h.finish()
    };
    format!("{:016X}{:016X}", part(1), part(2))
}

/// Inputs that never carry netusb play info.
fn is_external_input(id: &str) -> bool {
    let id = id.to_lowercase();
    ["optical", "coaxial", "hdmi", "aux", "av", "audio", "line", "phono", "tv", "cd", "analog", "usb_dac"]
        .iter()
        .any(|p| id.starts_with(p))
}

/// Human-friendly name for a raw MusicCast input id.
pub fn input_display_name(id: &str) -> String {
    let known = match id {
        "net_radio" => "Net Radio",
        "mc_link" => "MusicCast Link",
        "server" => "Media Server",
        "usb" => "USB",
        "bluetooth" => "Bluetooth",
        "airplay" => "AirPlay",
        "spotify" => "Spotify",
        "tidal" => "TIDAL",
        "deezer" => "Deezer",
        "qobuz" => "Qobuz",
        "napster" => "Napster",
        "juke" => "JUKE",
        "pandora" => "Pandora",
        "siriusxm" => "SiriusXM",
        "amazon_music" => "Amazon Music",
        "alexa" => "Alexa",
        "tuner" => "Tuner",
        "optical" => "Optical",
        "coaxial" => "Coaxial",
        "cd" => "CD",
        "phono" => "Phono",
        "tv" => "TV",
        "aux" => "AUX",
        "line_cd" => "Line (CD)",
        "main_sync" => "Main Zone Sync",
        _ => "",
    };
    if !known.is_empty() {
        return known.to_string();
    }
    // Generic: "hdmi1" -> "HDMI 1", "av_in" -> "Av In".
    let mut out = String::new();
    for word in id.split('_') {
        if !out.is_empty() {
            out.push(' ');
        }
        let (letters, digits): (String, String) = word.chars().partition(|c| !c.is_ascii_digit());
        let upper = matches!(letters.as_str(), "hdmi" | "av" | "usb" | "dac" | "tv" | "cd");
        if upper {
            out.push_str(&letters.to_uppercase());
        } else {
            let mut c = letters.chars();
            if let Some(f) = c.next() {
                out.push(f.to_ascii_uppercase());
                out.push_str(c.as_str());
            }
        }
        if !digits.is_empty() {
            out.push(' ');
            out.push_str(&digits);
        }
    }
    out
}

/// One entry of a netusb list browse (getListInfo).
#[derive(Clone, Debug, Default)]
pub struct ListEntry {
    pub text: String,
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
        match self {
            Self::English => english,
            Self::Dutch => dutch,
        }
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
        [config_path(), legacy_dir().join("speaker-control-config.json")]
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

/// Directory next to the executable; only read for migrating old settings
/// (writing there would modify the signed app bundle).
fn legacy_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Per-user application data directory.
pub fn app_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    let base = std::env::var_os("HOME").map(|p| PathBuf::from(p).join("Library/Application Support"));
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".config")));
    base.map(|p| p.join("Controller for MusicCast")).unwrap_or_else(std::env::temp_dir)
}

pub fn config_path() -> PathBuf {
    app_dir().join("settings.json")
}

pub fn history_path() -> PathBuf {
    app_dir().join("music-history.json")
}

pub fn load_history() -> Vec<HistoryRow> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Match on title+artist+album+duration, increment playCount, else insert.
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
        let _ = std::fs::create_dir_all(app_dir());
        let _ = std::fs::write(history_path(), s);
    }
}

/// Round a value to the nearest multiple of step (ties go down).
/// Steps below 2 leave the value unchanged.
pub fn step_val(v: i64, step: i64) -> i64 {
    if step >= 2 {
        let q = v.div_euclid(step) * step;
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
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    format!("{}", d.as_secs())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_rounds_to_nearest() {
        assert_eq!(step_val(87520, 50), 87500);
        assert_eq!(step_val(87530, 50), 87550);
        assert_eq!(step_val(-3, 2), -4);
        assert_eq!(step_val(7, 1), 7);
    }

    #[test]
    fn group_ids() {
        let id = new_group_id();
        assert_eq!(id.len(), 32);
        assert!(valid_group_id(&id));
        assert!(!valid_group_id("00000000000000000000000000000000"));
        assert!(!valid_group_id(""));
        assert_ne!(new_group_id(), new_group_id());
    }

    #[test]
    fn input_names() {
        assert_eq!(input_display_name("net_radio"), "Net Radio");
        assert_eq!(input_display_name("hdmi1"), "HDMI 1");
        assert_eq!(input_display_name("av_in"), "AV In");
    }

    #[test]
    fn input_types() {
        let mut d = Device { input: "optical".into(), ..Default::default() };
        assert_eq!(d.input_type(), "none");
        d.input = "spotify".into();
        assert!(d.on_netusb());
        d.inputs.push(InputItem { id: "spotify".into(), title: "Spotify".into(), play_info_type: "netusb".into() });
        assert!(d.on_netusb());
    }
}
