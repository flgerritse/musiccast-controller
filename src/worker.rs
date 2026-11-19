//! Background worker: owns the device list, runs discovery, polls the selected
//! device on the configured interval (default 8 s, range 1..20 s), executes commands and streams local files. The UI talks to it
//! through job/event channels.

use crate::model::*;
use crate::{api, discovery, stream};
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

pub enum Job {
    Discover,
    LoadSaved,
    AddManual(String),
    TestIp(String),
    Remove(String),
    Select(String),
    SetZone(String),
    Poll,
    /// Raw API path under /YamahaExtendedControl/v1, e.g. "/main/setVolume?volume=20".
    Cmd(String),
    SetRepeat(String),
    SetShuffle(bool),
    SetPollSecs(u32),
    SetPreferences { language: Language, appearance: Appearance },
    PlayPaths(Vec<PathBuf>),
    LocalNext,
    LocalPrev,
    LocalJump(usize),
    LoadArt(String),
    Browse { input: String, index: i64 },
    BrowseSelect(i64),
    BrowsePlay(i64),
    BrowseReturn,
}

pub enum Event {
    Devices(Vec<Device>),
    Selected(String),
    Art { key: String, bytes: Vec<u8> },
    Notice(String),
    LocalFiles(Vec<String>),
    Browse { entries: Vec<ListEntry>, base: i64, layer: i64, input: String },
    Searching(bool),
    IpTestResult(String),
}

pub struct Worker {
    rx: Receiver<Job>,
    tx: Sender<Event>,
    devices: Vec<Device>,
    selected: String,
    config: Config,
    server: Option<stream::FileServer>,
    local_files: Vec<PathBuf>,
    local_index: usize,
    last_art: String,
    browse_input: String,
    browse_base: i64,
    browse_layer: i64,
}

pub fn spawn(tx: Sender<Event>) -> Sender<Job> {
    let (job_tx, job_rx) = std::sync::mpsc::channel();
    let config = Config::load();
    let mut w = Worker {
        rx: job_rx,
        tx,
        devices: Vec::new(),
        selected: String::new(),
        config,
        server: None,
        local_files: Vec::new(),
        local_index: 0,
        last_art: String::new(),
        browse_input: String::new(),
        browse_base: 0,
        browse_layer: 0,
    };
    std::thread::spawn(move || {
        w.send_devices();
        loop {
            let wait = Duration::from_secs(w.config.update_seconds.clamp(1, 20) as u64);
            match w.rx.recv_timeout(wait) {
                Ok(Job::Discover) => w.discover(),
                Ok(Job::LoadSaved) => w.load_saved(),
                Ok(Job::AddManual(ip)) => w.add_manual(&ip),
                Ok(Job::TestIp(ip)) => {
                    let msg = match api::test_manual_ip(&ip) {
                        Ok(_) => "Test OK".to_string(),
                        Err(e) => e,
                    };
                    let _ = w.tx.send(Event::IpTestResult(msg));
                }
                Ok(Job::Remove(ip)) => w.remove(&ip),
                Ok(Job::Select(ip)) => w.select(&ip),
                Ok(Job::SetZone(zone)) => {
                    if let Some(d) = w.sel_mut() {
                        d.zone = zone;
                    }
                    let ip = w.selected.clone();
                    if !ip.is_empty() {
                        w.select(&ip);
                    }
                }
                Ok(Job::Poll) => w.poll_selected(),
                Ok(Job::Cmd(path)) => w.cmd(&path),
                Ok(Job::SetRepeat(target)) => w.set_repeat(&target),
                Ok(Job::SetShuffle(on)) => w.set_shuffle(on),
                Ok(Job::SetPollSecs(s)) => {
                    w.config.update_seconds = s.clamp(1, 20);
                    w.save_config();
                }
                Ok(Job::SetPreferences { language, appearance }) => {
                    w.config.language = language;
                    w.config.appearance = appearance;
                    w.save_config();
                }
                Ok(Job::PlayPaths(paths)) => w.play_paths(paths),
                Ok(Job::LocalNext) => w.local_step(1),
                Ok(Job::LocalPrev) => w.local_step(-1),
                Ok(Job::LocalJump(i)) => {
                    if i < w.local_files.len() {
                        w.local_index = i;
                        w.push_local();
                    }
                }
                Ok(Job::LoadArt(url)) => w.load_art(&url),
                Ok(Job::Browse { input, index }) => w.browse(&input, index),
                Ok(Job::BrowseSelect(i)) => w.browse_action("select", i),
                Ok(Job::BrowsePlay(i)) => w.browse_action("play", i),
                Ok(Job::BrowseReturn) => w.browse_return(),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => w.poll_selected(),
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
    });
    job_tx
}

impl Worker {
    fn send_devices(&self) {
        let _ = self.tx.send(Event::Devices(self.devices.clone()));
    }
    fn notice(&self, msg: impl Into<String>) {
        let _ = self.tx.send(Event::Notice(msg.into()));
    }
    fn save_config(&self) {
        if let Err(error) = self.config.save() {
            self.notice(format!("{}: {}", self.config.language.text("Could not save settings", "Instellingen opslaan mislukt"), error));
        }
    }
    fn sel(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.ip == self.selected)
    }
    fn sel_mut(&mut self) -> Option<&mut Device> {
        let ip = self.selected.clone();
        self.devices.iter_mut().find(|d| d.ip == ip)
    }

    fn discover(&mut self) {
        let _ = self.tx.send(Event::Searching(true));
        let locations = discovery::search(Duration::from_millis(2500));
        let mut added = 0;
        for loc in locations {
            if let Some(dev) = api::device_from_description(&loc, false) {
                if self.devices.iter().any(|d| d.ip == dev.ip) {
                    continue;
                }
                if !dev.icon_url.is_empty() {
                    self.load_art(&dev.icon_url);
                }
                self.devices.push(dev);
                added += 1;
            }
        }
        let _ = self.tx.send(Event::Searching(false));
        if added > 0 {
            self.send_devices();
            if self.selected.is_empty() {
                let first = self.devices[0].ip.clone();
                self.select(&first);
            }
            self.notice(format!("{} {}", added, self.config.language.text("speaker(s) found", "speaker(s) gevonden")));
        }
    }

    fn load_saved(&mut self) {
        let ips = self.config.manual_ips.clone();
        for ip in ips {
            match api::test_manual_ip(&ip) {
                Ok(dev) => {
                    if !self.devices.iter().any(|d| d.ip == dev.ip) {
                        if !dev.icon_url.is_empty() {
                            self.load_art(&dev.icon_url);
                        }
                        self.devices.push(dev);
                    }
                }
                Err(_) => {
                    // keep the entry; device may be off right now
                }
            }
        }
        self.send_devices();
        if self.selected.is_empty() {
            if let Some(first) = self.devices.first() {
                let ip = first.ip.clone();
                self.select(&ip);
            }
        }
    }

    fn add_manual(&mut self, ip: &str) {
        let ip = ip.trim().to_string();
        if ip.is_empty() {
            return;
        }
        self.notice(format!("{} {} ...", self.config.language.text("Testing", "Testen"), ip));
        match api::test_manual_ip(&ip) {
            Ok(dev) => {
                self.notice(self.config.language.text("Speaker found", "Speaker gevonden"));
                if !self.config.manual_ips.contains(&ip) {
                    self.config.manual_ips.push(ip.clone());
                    self.save_config();
                }
                if !self.devices.iter().any(|d| d.ip == dev.ip) {
                    if !dev.icon_url.is_empty() {
                        self.load_art(&dev.icon_url);
                    }
                    self.devices.push(dev);
                }
                self.send_devices();
                self.select(&ip);
            }
            Err(e) => self.notice(format!("{}: {}", self.config.language.text("Test failed", "Test mislukt"), e)),
        }
    }

    fn remove(&mut self, ip: &str) {
        self.devices.retain(|d| d.ip != ip);
        self.config.manual_ips.retain(|s| s != ip);
        self.save_config();
        if self.selected == ip {
            self.selected = self.devices.first().map(|d| d.ip.clone()).unwrap_or_default();
            let _ = self.tx.send(Event::Selected(self.selected.clone()));
            if !self.selected.is_empty() {
                let ip = self.selected.clone();
                self.select(&ip);
            }
        }
        self.send_devices();
    }

    /// initDevice: getFeatures -> getMusicProgram -> updatePreset -> updateInfo.
    fn select(&mut self, ip: &str) {
        self.selected = ip.to_string();
        let _ = self.tx.send(Event::Selected(self.selected.clone()));
        let Some(dev) = self.sel().cloned() else { return };
        let mut dev = dev;
        if let Some(v) = api::get_json(&dev, "/system/getFeatures") {
            api::parse_features(&mut dev, &v);
        }
        if let Some(v) = api::get_json(&dev, "/main/getSoundProgramList") {
            api::parse_programs(&mut dev, &v);
        }
        // Publish features/programs promptly; slow preset endpoints must not
        // blank the tables.
        if let Some(d) = self.sel_mut() {
            *d = dev.clone();
        }
        self.send_devices();
        // netusb presets
        if let Some(v) = api::get_json(&dev, "/netusb/getPresetInfo") {
            dev.presets = api::parse_presets(&v, "");
        }
        // tuner presets per band
        dev.tuner_presets.clear();
        for band in dev.bands.clone() {
            if let Some(v) = api::get_json(&dev, &format!("/tuner/getPresetInfo?band={}", band)) {
                dev.tuner_presets.extend(api::parse_presets(&v, &band));
            }
        }
        if let Some(d) = self.sel_mut() {
            *d = dev.clone();
        }
        self.send_devices();
        if let Some(v) = api::get_json(&dev, "/system/getFuncStatus") {
            api::parse_func_status(&mut dev, &v);
        }
        if let Some(d) = self.sel_mut() {
            *d = dev;
        }
        self.send_devices();
        self.poll_selected();
    }

    fn poll_selected(&mut self) {
        let Some(dev) = self.sel().cloned() else { return };
        let mut dev = dev;
        let mut alive = false;
        if let Some(v) = api::get_json(&dev, "/main/getStatus") {
            api::parse_status(&mut dev, &v);
            alive = true;
            if let Some(sig) = api::get_json(&dev, "/main/getSignalInfo") {
                api::parse_signal_info(&mut dev, &sig);
            }
        }
        if alive {
            if dev.input == "tuner" {
                dev.track.clear();
                dev.artist.clear();
                dev.album.clear();
                dev.art_url.clear();
                if let Some(v) = api::get_json(&dev, "/tuner/getPlayInfo") {
                    api::parse_tuner_info(&mut dev, &v);
                }
            } else if let Some(v) = api::get_json(&dev, "/netusb/getPlayInfo") {
                api::parse_play_info(&mut dev, &v);
            }
            if let Some(v) = api::get_json(&dev, "/system/getFuncStatus") {
                api::parse_func_status(&mut dev, &v);
            }
        }
        dev.alive = alive;
        let art = dev.art_url.clone();
        if let Some(d) = self.sel_mut() {
            *d = dev;
        }
        self.send_devices();
        if !art.is_empty() && art != self.last_art {
            self.last_art = art.clone();
            self.load_art(&art);
        }
    }

    fn cmd(&mut self, path: &str) {
        let Some(dev) = self.sel().cloned() else { return };
        let success = api::command(&dev, path);
        if !success {
            self.notice(self.config.language.text("Command failed. Check the speaker connection.", "Opdracht mislukt. Controleer de verbinding met de speaker."));
        } else if path.contains("storePreset") {
            self.notice(self.config.language.text("Save command sent to speaker", "Opslagopdracht naar speaker verzonden"));
            self.refresh_presets();
        }
        // let the device settle, then refresh
        std::thread::sleep(Duration::from_millis(400));
        self.poll_selected();
        if path.contains("setPower") {
            std::thread::sleep(Duration::from_millis(1500));
            self.poll_selected();
        }
    }

    fn refresh_presets(&mut self) {
        let Some(mut dev) = self.sel().cloned() else { return };
        if let Some(v) = api::get_json(&dev, "/netusb/getPresetInfo") {
            dev.presets = api::parse_presets(&v, "");
        }
        let mut tuner_presets = Vec::new();
        for band in dev.bands.clone() {
            if let Some(v) = api::get_json(&dev, &format!("/tuner/getPresetInfo?band={band}")) {
                tuner_presets.extend(api::parse_presets(&v, &band));
            }
        }
        if !tuner_presets.is_empty() { dev.tuner_presets = tuner_presets; }
        if let Some(d) = self.sel_mut() { *d = dev; }
        self.send_devices();
    }

    /// toggleRepeat until the reported state matches (repeatOff/One/All handlers).
    fn set_repeat(&mut self, target: &str) {
        for _ in 0..3 {
            let Some(dev) = self.sel().cloned() else { return };
            if dev.repeat == target {
                break;
            }
            if !api::command(&dev, "/netusb/toggleRepeat") {
                self.notice(self.config.language.text("Could not change repeat mode", "Herhaalstand wijzigen mislukt"));
                break;
            }
            std::thread::sleep(Duration::from_millis(400));
            self.poll_selected();
        }
    }

    fn set_shuffle(&mut self, on: bool) {
        let Some(dev) = self.sel().cloned() else { return };
        if dev.shuffle != on {
            if !api::command(&dev, "/netusb/toggleShuffle") {
                self.notice(self.config.language.text("Could not change shuffle mode", "Willekeurige volgorde wijzigen mislukt"));
                return;
            }
            std::thread::sleep(Duration::from_millis(400));
            self.poll_selected();
        }
    }

    fn play_paths(&mut self, paths: Vec<PathBuf>) {
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| stream::is_supported(p)).collect();
        if files.is_empty() {
            self.notice(self.config.language.text("No supported music files (mp3, flac, wav, m4a, ...)", "Geen ondersteunde muziekbestanden (mp3, flac, wav, m4a, ...)"));
            return;
        }
        if self.server.is_none() {
            self.server = stream::FileServer::start();
            if self.server.is_none() {
                self.notice(self.config.language.text("Could not start the local file server", "Lokale bestandsserver starten mislukt"));
                return;
            }
        }
        let server = self.server.as_ref().unwrap();
        server.set_files(files.clone());
        self.local_files = files;
        self.local_index = 0;
        let names: Vec<String> = self
            .local_files
            .iter()
            .map(|p| p.file_name().and_then(|n| n.to_str()).unwrap_or("?").to_string())
            .collect();
        let _ = self.tx.send(Event::LocalFiles(names));
        self.push_local();
    }

    fn local_step(&mut self, delta: i64) {
        if self.local_files.is_empty() {
            return;
        }
        let n = self.local_files.len() as i64;
        let next = (self.local_index as i64 + delta).clamp(0, n - 1) as usize;
        self.local_index = next;
        self.push_local();
    }

    fn push_local(&mut self) {
        let Some(dev) = self.sel().cloned() else {
            self.notice(self.config.language.text("Select a speaker first", "Selecteer eerst een speaker"));
            return;
        };
        let Some(path) = self.local_files.get(self.local_index).cloned() else { return };
        let server = match self.server.as_ref() {
            Some(s) => s,
            None => return,
        };
        let url = server.url_for(&path);
        let title = path.file_stem().and_then(|n| n.to_str()).unwrap_or("Unknown").to_string();
        let mime = stream::mime_for(&path);
        // Switch to the DLNA ("server") input, then push.
        if !dev.dlna_input.is_empty() && dev.input != dev.dlna_input {
            api::command(&dev, &format!("/main/setInput?input={}", dev.dlna_input));
            std::thread::sleep(Duration::from_millis(600));
        }
        match stream::dlna_push(&dev.dlna_url, &url, &title, mime) {
            Ok(()) => {
                self.notice(format!("{}: {}", self.config.language.text("Streaming", "Afspelen"), title));
                record_play(HistoryRow {
                    title: title.clone(),
                    artist: String::new(),
                    album: String::new(),
                    duration: 0,
                    mediauri: url,
                    play_count: 1,
                    last_update: now_string(),
                    speaker_name: dev.display_name(),
                    speaker_ip: dev.ip.clone(),
                });
                std::thread::sleep(Duration::from_millis(800));
                self.poll_selected();
            }
            Err(e) => self.notice(format!("{}: {}", self.config.language.text("DLNA playback failed", "DLNA afspelen mislukt"), e)),
        }
    }

    fn load_art(&mut self, url: &str) {
        let resp = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(6))
            .build()
            .get(url)
            .call();
        if let Ok(r) = resp {
            let mut buf = Vec::new();
            use std::io::Read;
            if r.into_reader().take(8 * 1024 * 1024).read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                let _ = self.tx.send(Event::Art { key: url.to_string(), bytes: buf });
            }
        }
    }

    fn browse(&mut self, input: &str, index: i64) {
        let Some(dev) = self.sel().cloned() else { return };
        self.browse_input = input.to_string();
        self.browse_base = index;
        if let Some(v) = api::get_json(&dev, &format!("/netusb/getListInfo?input={}&size=8&index={}", input, index)) {
            let (entries, _max, layer) = api::parse_list(&v);
            self.browse_layer = layer;
            let _ = self.tx.send(Event::Browse { entries, base: index, layer, input: input.to_string() });
        }
    }

    fn browse_action(&mut self, kind: &str, index: i64) {
        let Some(dev) = self.sel().cloned() else { return };
        let input = self.browse_input.clone();
        let url = if kind == "select" {
            format!("/netusb/setListControl?input={}&list_id=main&type=select&index={}&size=8", input, index)
        } else {
            format!("/netusb/setListControl?input={}&list_id=main&type=play&index={}", input, index)
        };
        api::command(&dev, &url);
        std::thread::sleep(Duration::from_millis(500));
        if kind == "select" {
            self.browse(&input, index);
        } else {
            self.poll_selected();
        }
    }

    fn browse_return(&mut self) {
        let Some(dev) = self.sel().cloned() else { return };
        api::command(&dev, "/netusb/setListControl?list_id=main&type=return");
        std::thread::sleep(Duration::from_millis(500));
        let input = self.browse_input.clone();
        let base = (self.browse_base - 8).max(0);
        self.browse(&input, base);
    }
}