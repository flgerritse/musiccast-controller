//! Background worker: owns the device list, polls the selected device on the
//! configured interval (default 8 s, range 1..20 s), executes commands and
//! streams local files. Slow work (discovery, probing saved speakers, album art,
//! the status of the other speakers) runs on helper threads that report back
//! through the job channel, so commands are never stuck behind it. Speakers also
//! push UDP change notifications, which trigger an immediate refresh.
//!
//! The UI talks to the worker through job/event channels.

use crate::model::*;
use crate::{api, discovery, stream};
use eframe::egui;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

pub enum Job {
    Discover,
    LoadSaved,
    AddManual(String),
    TestIp(String),
    Remove(String),
    Select(String),
    SetZone(String),
    /// Raw API path under /YamahaExtendedControl/v1, e.g. "/main/setMute?enable=true".
    Cmd(String),
    /// Coalesced: only the last volume of a burst is sent.
    SetVolume(i64),
    SetPower(bool),
    SetRepeat(String),
    SetShuffle(bool),
    SetPollSecs(u32),
    SetPreferences { language: Language, appearance: Appearance },
    PlayPaths(Vec<PathBuf>),
    LocalNext,
    LocalPrev,
    LocalJump(usize),
    Browse { input: String, index: i64 },
    BrowseSelect(i64),
    BrowsePlay(i64),
    BrowseReturn,
    // Results from helper threads.
    Found { devices: Vec<Device>, offline: Vec<String>, discovery: bool },
    /// Fresh state of a non-selected speaker; `None` marks the end of a round.
    OtherStatus(Option<Box<Device>>),
    /// Command for a speaker other than the selected one (group member volume).
    CmdFor { ip: String, path: String },
    /// MusicCast Link: the selected speaker shares its music with these speakers.
    LinkAdd(Vec<String>),
    /// MusicCast Link: this speaker stops following its group.
    LinkRemove(String),
    /// MusicCast Link: end the group of the selected speaker.
    LinkDissolve,
    DeviceEvent { ip: String, play_time: Option<i64>, refresh: bool },
}

pub enum Event {
    Devices(Vec<Device>),
    Selected(String),
    Art { key: String, bytes: Vec<u8> },
    Notice(String),
    LocalFiles { names: Vec<String>, current: Option<usize> },
    Browse { page: api::ListPage, input: String },
    BrowseError(String),
    Searching(bool),
    IpTestResult(Result<String, String>),
}

/// Event sender that also wakes the UI, so it only repaints when something changed.
#[derive(Clone)]
pub struct EventTx {
    tx: Sender<Event>,
    ctx: Arc<OnceLock<egui::Context>>,
}

impl EventTx {
    pub fn new(tx: Sender<Event>, ctx: Arc<OnceLock<egui::Context>>) -> Self {
        Self { tx, ctx }
    }
    pub fn send(&self, ev: Event) {
        let _ = self.tx.send(ev);
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
            ctx.request_repaint_of(crate::ui::player_viewport_id());
        }
    }
}

pub struct Worker {
    rx: Receiver<Job>,
    self_tx: Sender<Job>,
    tx: EventTx,
    art_tx: Sender<String>,
    art_requested: HashSet<String>,
    devices: Vec<Device>,
    selected: String,
    config: Config,
    discovering: bool,
    poll_at: Instant,
    extra_poll: Option<Instant>,
    others_at: Instant,
    others_busy: bool,
    server: Option<stream::FileServer>,
    local_files: Vec<PathBuf>,
    local_index: usize,
    /// A pushed local file is playing; when it stops, advance to the next one.
    local_active: bool,
    local_saw_play: bool,
    browse_input: String,
}

const OTHERS_INTERVAL: Duration = Duration::from_secs(30);

pub fn spawn(tx: EventTx) -> Sender<Job> {
    let (job_tx, job_rx) = std::sync::mpsc::channel();
    let config = Config::load();
    let art_tx = spawn_art_loader(tx.clone());
    spawn_event_listener(job_tx.clone());
    let now = Instant::now();
    let mut w = Worker {
        rx: job_rx,
        self_tx: job_tx.clone(),
        tx,
        art_tx,
        art_requested: HashSet::new(),
        devices: Vec::new(),
        selected: String::new(),
        config,
        discovering: false,
        poll_at: now + Duration::from_secs(1),
        extra_poll: None,
        others_at: now + Duration::from_secs(10),
        others_busy: false,
        server: None,
        local_files: Vec::new(),
        local_index: 0,
        local_active: false,
        local_saw_play: false,
        browse_input: String::new(),
    };
    std::thread::spawn(move || w.run());
    job_tx
}

/// Album art and device icons are fetched on their own thread.
fn spawn_art_loader(tx: EventTx) -> Sender<String> {
    let (art_tx, art_rx) = std::sync::mpsc::channel::<String>();
    std::thread::spawn(move || {
        for key in art_rx {
            // The key may carry "#albumart_id" to tell images with one URL apart.
            let url = key.split('#').next().unwrap_or(&key);
            let Ok(resp) = api::agent().get(url).timeout(Duration::from_secs(6)).call() else { continue };
            let mut buf = Vec::new();
            use std::io::Read;
            if resp.into_reader().take(8 * 1024 * 1024).read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                tx.send(Event::Art { key, bytes: buf });
            }
        }
    });
    art_tx
}

/// Listen for MusicCast UDP event notifications. Every API request carries
/// X-AppPort with this socket's port, which keeps the subscription alive.
fn spawn_event_listener(job_tx: Sender<Job>) {
    let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:41100").or_else(|_| std::net::UdpSocket::bind("0.0.0.0:0")) else {
        return;
    };
    let Ok(addr) = sock.local_addr() else { return };
    api::set_event_port(addr.port());
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let Ok((n, from)) = sock.recv_from(&mut buf) else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(&buf[..n]) else { continue };
            let Some(obj) = v.as_object() else { continue };
            let play_time = v.get("netusb").and_then(|n| n.get("play_time")).and_then(|t| t.as_i64());
            // The device sends play_time every second while playing; anything
            // else (volume, input, track change, ...) warrants a refresh.
            let only_time = obj.iter().all(|(k, val)| {
                k == "device_id" || (k == "netusb" && val.as_object().is_some_and(|o| o.keys().all(|k| k == "play_time")))
            });
            let job = Job::DeviceEvent { ip: from.ip().to_string(), play_time, refresh: !only_time };
            if job_tx.send(job).is_err() {
                break;
            }
        }
    });
}

impl Worker {
    fn run(&mut self) {
        self.send_devices();
        loop {
            let now = Instant::now();
            let mut deadline = self.poll_at.min(self.others_at);
            if let Some(extra) = self.extra_poll {
                deadline = deadline.min(extra);
            }
            match self.rx.recv_timeout(deadline.saturating_duration_since(now)) {
                Ok(job) => {
                    let mut jobs = vec![job];
                    while let Ok(j) = self.rx.try_recv() {
                        jobs.push(j);
                    }
                    // Volume drags and key repeats: only the most recent value matters.
                    let last_volume = jobs.iter().rposition(|j| matches!(j, Job::SetVolume(_)));
                    for (i, job) in jobs.into_iter().enumerate() {
                        if matches!(job, Job::SetVolume(_)) && Some(i) != last_volume {
                            continue;
                        }
                        self.handle(job);
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
            let now = Instant::now();
            if self.extra_poll.is_some_and(|t| now >= t) {
                self.extra_poll = None;
                self.poll_selected();
            } else if now >= self.poll_at {
                self.poll_selected();
            }
            if now >= self.others_at {
                self.check_others();
            }
        }
    }

    fn handle(&mut self, job: Job) {
        match job {
            Job::Discover => self.discover(),
            Job::LoadSaved => self.load_saved(),
            Job::AddManual(ip) => self.add_manual(&ip),
            Job::TestIp(ip) => {
                let tx = self.tx.clone();
                std::thread::spawn(move || {
                    let res = api::test_manual_ip(&ip).map(|d| d.display_name());
                    tx.send(Event::IpTestResult(res));
                });
            }
            Job::Remove(ip) => self.remove(&ip),
            Job::Select(ip) => self.select(&ip),
            Job::SetZone(zone) => {
                if let Some(d) = self.sel_mut() {
                    d.zone = zone;
                }
                let ip = self.selected.clone();
                if !ip.is_empty() {
                    self.select(&ip);
                }
            }
            Job::Cmd(path) => self.cmd(&path),
            Job::SetVolume(v) => {
                if let Some(d) = self.sel_mut() {
                    d.volume = v;
                }
                let path = match self.sel() {
                    Some(d) => d.zpath(&format!("setVolume?volume={v}")),
                    None => return,
                };
                self.cmd(&path);
            }
            Job::SetPower(on) => {
                let Some(path) = self.sel().map(|d| d.zpath(&format!("setPower?power={}", if on { "on" } else { "standby" })))
                else {
                    return;
                };
                self.cmd(&path);
                // Inputs and features settle a moment after power-on.
                self.extra_poll = Some(Instant::now() + Duration::from_secs(2));
            }
            Job::SetRepeat(target) => self.set_mode(true, &target),
            Job::SetShuffle(on) => {
                let target = match (on, self.sel()) {
                    (false, _) => "off".to_string(),
                    (true, Some(d)) => d
                        .shuffle_modes
                        .iter()
                        .find(|m| m.as_str() == "on")
                        .or_else(|| d.shuffle_modes.iter().find(|m| m.as_str() != "off"))
                        .cloned()
                        .unwrap_or_else(|| "on".into()),
                    (true, None) => return,
                };
                self.set_mode(false, &target);
            }
            Job::SetPollSecs(s) => {
                self.config.update_seconds = s.clamp(1, 20);
                self.save_config();
                self.poll_at = self.poll_at.min(Instant::now() + self.interval());
            }
            Job::SetPreferences { language, appearance } => {
                self.config.language = language;
                self.config.appearance = appearance;
                self.save_config();
            }
            Job::PlayPaths(paths) => self.play_paths(paths),
            Job::LocalNext => self.local_step(1),
            Job::LocalPrev => self.local_step(-1),
            Job::LocalJump(i) => {
                if i < self.local_files.len() {
                    self.local_index = i;
                    self.push_local();
                }
            }
            Job::Browse { input, index } => self.browse(&input, index),
            Job::BrowseSelect(i) => self.browse_control("select", i),
            Job::BrowsePlay(i) => self.browse_control("play", i),
            Job::BrowseReturn => self.browse_control("return", 0),
            Job::Found { devices, offline, discovery } => self.found(devices, offline, discovery),
            Job::OtherStatus(None) => {
                self.others_busy = false;
                self.send_devices();
            }
            Job::OtherStatus(Some(probe)) => {
                if probe.ip != self.selected {
                    if let Some(d) = self.devices.iter_mut().find(|d| d.ip == probe.ip) {
                        *d = *probe;
                    }
                }
            }
            Job::CmdFor { ip, path } => {
                let Some(dev) = self.devices.iter().find(|d| d.ip == ip).cloned() else { return };
                if let Err(e) = api::command(&dev, &path) {
                    self.notice(format!("{}: {} ({e})", dev.display_name(), self.t("command failed", "opdracht mislukt")));
                }
                self.refresh_others_soon();
            }
            Job::LinkAdd(ips) => self.link_add(ips),
            Job::LinkRemove(ip) => self.link_remove(&ip),
            Job::LinkDissolve => {
                if let Some(server) = self.sel().cloned() {
                    self.link_dissolve(&server);
                    self.notice(self.t("Group ended", "Groep opgeheven"));
                    self.after_link_change();
                }
            }
            Job::DeviceEvent { ip, play_time, refresh } => {
                if ip != self.selected {
                    // Cheap: the periodic status check of the other speakers picks it up.
                    if refresh {
                        self.refresh_others_soon();
                    }
                    return;
                }
                if let Some(t) = play_time {
                    if let Some(d) = self.sel_mut() {
                        d.play_time = t;
                    }
                    self.send_devices();
                }
                if refresh {
                    // Debounce: a volume drag sends a burst of events.
                    self.poll_at = self.poll_at.min(Instant::now() + Duration::from_millis(150));
                }
            }
        }
    }

    fn t<'a>(&self, en: &'a str, nl: &'a str) -> &'a str {
        self.config.language.text(en, nl)
    }
    fn interval(&self) -> Duration {
        Duration::from_secs(self.config.update_seconds.clamp(1, 20) as u64)
    }
    fn send_devices(&self) {
        self.tx.send(Event::Devices(self.devices.clone()));
    }
    fn notice(&self, msg: impl Into<String>) {
        self.tx.send(Event::Notice(msg.into()));
    }
    fn save_config(&self) {
        if let Err(error) = self.config.save() {
            self.notice(format!("{}: {}", self.t("Could not save settings", "Instellingen opslaan mislukt"), error));
        }
    }
    fn sel(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.ip == self.selected)
    }
    fn sel_mut(&mut self) -> Option<&mut Device> {
        let ip = self.selected.clone();
        self.devices.iter_mut().find(|d| d.ip == ip)
    }
    fn schedule_poll(&mut self, after: Duration) {
        self.poll_at = self.poll_at.min(Instant::now() + after);
    }
    fn load_art(&mut self, key: &str) {
        if !key.is_empty() && self.art_requested.insert(key.to_string()) {
            let _ = self.art_tx.send(key.to_string());
        }
    }

    fn discover(&mut self) {
        if self.discovering {
            return;
        }
        self.discovering = true;
        self.tx.send(Event::Searching(true));
        let jobs = self.self_tx.clone();
        std::thread::spawn(move || {
            let locations = discovery::search(Duration::from_millis(2500));
            // Fetch the descriptions in parallel: one slow device must not hold up the rest.
            let handles: Vec<_> = locations
                .into_iter()
                .map(|loc| std::thread::spawn(move || api::device_from_description(&loc, false)))
                .collect();
            let devices = handles.into_iter().filter_map(|h| h.join().ok().flatten()).collect();
            let _ = jobs.send(Job::Found { devices, offline: Vec::new(), discovery: true });
        });
    }

    fn load_saved(&mut self) {
        let ips = self.config.manual_ips.clone();
        if ips.is_empty() {
            return;
        }
        let jobs = self.self_tx.clone();
        std::thread::spawn(move || {
            let handles: Vec<_> =
                ips.into_iter().map(|ip| std::thread::spawn(move || (api::test_manual_ip(&ip), ip))).collect();
            let mut devices = Vec::new();
            let mut offline = Vec::new();
            for h in handles {
                match h.join() {
                    Ok((Ok(dev), _)) => devices.push(dev),
                    Ok((Err(_), ip)) => offline.push(ip),
                    Err(_) => {}
                }
            }
            let _ = jobs.send(Job::Found { devices, offline, discovery: false });
        });
    }

    /// Merge devices found by discovery or by probing saved addresses.
    fn found(&mut self, devices: Vec<Device>, offline: Vec<String>, discovery: bool) {
        let mut added = 0;
        for mut dev in devices {
            if let Some(existing) = self.devices.iter_mut().find(|d| d.ip == dev.ip) {
                existing.alive = true;
                if existing.dlna_url.is_empty() {
                    // Placeholder of a saved speaker that was offline: fill it in.
                    dev.is_manual = true;
                    dev.zone = existing.zone.clone();
                    *existing = dev;
                }
                continue;
            }
            if self.config.manual_ips.contains(&dev.ip) {
                dev.is_manual = true;
            }
            let icon = dev.icon_url.clone();
            self.devices.push(dev);
            self.load_art(&icon);
            added += 1;
        }
        // Saved speakers that are off right now stay listed (and removable).
        for ip in offline {
            if !self.devices.iter().any(|d| d.ip == ip) {
                self.devices.push(Device {
                    base_url: format!("http://{ip}"),
                    ip,
                    is_manual: true,
                    alive: false,
                    zone: "main".into(),
                    volume_max: 100,
                    volume_step: 1,
                    ..Default::default()
                });
            }
        }
        if discovery {
            self.discovering = false;
            self.tx.send(Event::Searching(false));
            if added > 0 {
                self.notice(format!("{} {}", added, self.t("speaker(s) found", "speaker(s) gevonden")));
            }
        }
        self.send_devices();
        if self.selected.is_empty() || self.sel().is_none() {
            // Prefer a speaker that is actually reachable.
            let first = self.devices.iter().find(|d| d.alive).or(self.devices.first()).map(|d| d.ip.clone());
            if let Some(ip) = first {
                self.select(&ip);
            }
        }
    }

    fn add_manual(&mut self, host: &str) {
        let host = host.trim().to_string();
        if host.is_empty() {
            return;
        }
        self.notice(format!("{} {host} …", self.t("Testing", "Testen")));
        match api::test_manual_ip(&host) {
            Ok(dev) => {
                self.notice(self.t("Speaker found", "Speaker gevonden"));
                let ip = dev.ip.clone();
                if !self.config.manual_ips.contains(&ip) {
                    self.config.manual_ips.push(ip.clone());
                    self.save_config();
                }
                self.found(vec![dev], Vec::new(), false);
                self.select(&ip);
            }
            Err(e) => self.notice(format!("{}: {e}", self.t("Test failed", "Test mislukt"))),
        }
    }

    fn remove(&mut self, ip: &str) {
        self.devices.retain(|d| d.ip != ip);
        if self.config.manual_ips.iter().any(|s| s == ip) {
            self.config.manual_ips.retain(|s| s != ip);
            self.save_config();
        }
        if self.selected == ip {
            self.selected.clear();
            self.local_active = false;
            match self.devices.first().map(|d| d.ip.clone()) {
                Some(next) => self.select(&next),
                None => self.tx.send(Event::Selected(String::new())),
            }
        }
        self.send_devices();
    }

    /// Read features, sound programs and presets, then the current status.
    fn select(&mut self, ip: &str) {
        if self.selected != ip {
            self.local_active = false;
        }
        self.selected = ip.to_string();
        self.tx.send(Event::Selected(self.selected.clone()));
        let Some(mut dev) = self.sel().cloned() else { return };
        match api::get_json(&dev, "/system/getFeatures") {
            Some(v) => api::parse_features(&mut dev, &v),
            None => {
                // Offline: don't wait for every other endpoint to time out as well.
                dev.alive = false;
                if let Some(d) = self.sel_mut() {
                    *d = dev;
                }
                self.send_devices();
                self.poll_at = Instant::now() + self.interval();
                return;
            }
        }
        if let Some(v) = api::get_json(&dev, "/system/getNameText") {
            api::parse_name_text(&mut dev, &v);
        }
        if let Some(v) = api::get_json(&dev, &dev.zpath("getSoundProgramList")) {
            api::parse_programs(&mut dev, &v);
        }
        // Publish features promptly; presets follow.
        if let Some(d) = self.sel_mut() {
            *d = dev;
        }
        self.send_devices();
        self.refresh_presets();
        if let Some(v) = self.sel().and_then(|d| api::get_json(d, "/system/getFuncStatus")) {
            if let Some(d) = self.sel_mut() {
                api::parse_func_status(d, &v);
            }
        }
        self.poll_selected();
    }

    fn poll_selected(&mut self) {
        self.poll_at = Instant::now() + self.interval();
        let Some(mut dev) = self.sel().cloned() else { return };
        let mut alive = false;
        if let Some(v) = api::get_json(&dev, &dev.zpath("getStatus")) {
            api::parse_status(&mut dev, &v);
            alive = true;
            if let Some(sig) = api::get_json(&dev, &dev.zpath("getSignalInfo")) {
                api::parse_signal_info(&mut dev, &sig);
            }
        }
        if alive && !dev.initialised {
            // A saved speaker came back online: read its features first.
            dev.alive = true;
            if let Some(d) = self.sel_mut() {
                *d = dev;
            }
            let ip = self.selected.clone();
            self.select(&ip);
            return;
        }
        if alive {
            if dev.on_tuner() {
                clear_track(&mut dev);
                if let Some(v) = api::get_json(&dev, "/tuner/getPlayInfo") {
                    api::parse_tuner_info(&mut dev, &v);
                }
            } else if dev.on_netusb() {
                if let Some(v) = api::get_json(&dev, "/netusb/getPlayInfo") {
                    api::parse_play_info(&mut dev, &v);
                }
            } else {
                // External input (optical, HDMI, ...): there is no track info.
                clear_track(&mut dev);
            }
            if let Some(v) = api::get_json(&dev, "/dist/getDistributionInfo") {
                api::parse_distribution(&mut dev, &v);
            }
        }
        dev.alive = alive;
        self.track_local_playback(&dev);
        let art = dev.art_url.clone();
        if let Some(d) = self.sel_mut() {
            *d = dev;
        }
        self.send_devices();
        self.load_art(&art);
    }

    /// Light status check of the non-selected speakers, on a helper thread.
    /// Saved speakers that were offline are probed again.
    fn check_others(&mut self) {
        self.others_at = Instant::now() + OTHERS_INTERVAL;
        if self.others_busy {
            return;
        }
        let others: Vec<Device> = self.devices.iter().filter(|d| d.ip != self.selected).cloned().collect();
        if others.is_empty() {
            return;
        }
        self.others_busy = true;
        let jobs = self.self_tx.clone();
        std::thread::spawn(move || {
            let handles: Vec<_> = others
                .into_iter()
                .map(|d| {
                    let jobs = jobs.clone();
                    std::thread::spawn(move || {
                        if d.dlna_url.is_empty() {
                            if let Ok(dev) = api::test_manual_ip(&d.ip) {
                                let _ = jobs.send(Job::Found { devices: vec![dev], offline: Vec::new(), discovery: false });
                                return;
                            }
                        }
                        let mut probe = d.clone();
                        let status = api::get_json(&d, &d.zpath("getStatus"));
                        probe.alive = status.is_some();
                        if let Some(v) = &status {
                            // Volume range and inputs are needed for the group controls.
                            if !probe.initialised {
                                if let Some(f) = api::get_json(&probe, "/system/getFeatures") {
                                    api::parse_features(&mut probe, &f);
                                }
                            }
                            api::parse_status(&mut probe, v);
                            if let Some(dist) = api::get_json(&probe, "/dist/getDistributionInfo") {
                                api::parse_distribution(&mut probe, &dist);
                            }
                        }
                        let _ = jobs.send(Job::OtherStatus(Some(Box::new(probe))));
                    })
                })
                .collect();
            for h in handles {
                let _ = h.join();
            }
            // Signals completion; the empty ip matches no device.
            let _ = jobs.send(Job::OtherStatus(None));
        });
    }

    fn refresh_others_soon(&mut self) {
        self.others_at = self.others_at.min(Instant::now() + Duration::from_secs(1));
    }

    fn after_link_change(&mut self) {
        self.schedule_poll(Duration::from_millis(500));
        self.refresh_others_soon();
    }

    fn device(&self, ip: &str) -> Option<Device> {
        self.devices.iter().find(|d| d.ip == ip).cloned()
    }

    /// MusicCast Link: the selected speaker (server) shares its music with
    /// `ips` (clients). Clients are told who their server is first, then the
    /// server adds them and (re)starts the distribution.
    fn link_add(&mut self, ips: Vec<String>) {
        let Some(mut server) = self.sel().cloned() else { return };
        if server.is_link_client() {
            // A follower cannot share; take it out of its group first.
            self.link_remove(&server.ip.clone());
            server.link_role.clear();
            server.link_group_id.clear();
        }
        self.notice(self.t("Grouping speakers…", "Speakers groeperen…"));
        if !server.power_on {
            let _ = api::command(&server, &server.zpath("setPower?power=on"));
            std::thread::sleep(Duration::from_millis(1200));
        }
        let group_id = if server.link_role == "server" && valid_group_id(&server.link_group_id) {
            server.link_group_id.clone()
        } else {
            new_group_id()
        };
        let mut joined = Vec::new();
        for ip in ips {
            let Some(client) = self.device(&ip) else { continue };
            // A speaker follows one server at a time.
            if client.is_link_server() {
                self.link_dissolve(&client);
            } else if client.is_link_client() {
                self.link_remove(&client.ip);
            }
            if !client.power_on {
                let _ = api::command(&client, &client.zpath("setPower?power=on"));
            }
            let body = json!({"group_id": group_id, "zone": [client.zone_or_main()], "server_ip_address": server.ip});
            match api::post_json(&client, "/dist/setClientInfo", &body) {
                Ok(()) => {
                    // Most firmware switches by itself; this makes sure.
                    let _ = api::command(&client, &client.zpath("setInput?input=mc_link"));
                    joined.push(ip);
                }
                Err(e) => self.notice(format!("{}: {} ({e})", client.display_name(), self.t("could not join", "kon niet aansluiten"))),
            }
        }
        if joined.is_empty() {
            return;
        }
        let body = json!({"group_id": group_id, "zone": server.zone_or_main(), "type": "add", "client_list": joined});
        let result = api::post_json(&server, "/dist/setServerInfo", &body)
            .and_then(|_| api::command(&server, "/dist/startDistribution?num=0"));
        match result {
            Ok(()) => {
                if !valid_group_id(&server.link_group_id) || server.link_group_name.is_empty() {
                    let _ = api::post_json(&server, "/dist/setGroupName", &json!({"name": server.display_name()}));
                }
                self.notice(self.t("Speakers grouped", "Speakers gegroepeerd"));
            }
            Err(e) => self.notice(format!("{} ({e})", self.t("Grouping failed", "Groeperen mislukt"))),
        }
        self.after_link_change();
    }

    /// Take `ip` out of its group. If it is the server, the group ends.
    fn link_remove(&mut self, ip: &str) {
        let Some(client) = self.device(ip) else { return };
        if client.is_link_server() {
            self.link_dissolve(&client);
            self.after_link_change();
            return;
        }
        let server = self
            .devices
            .iter()
            .find(|d| d.link_clients.iter().any(|c| c == ip) || (d.is_link_server() && d.link_group_id == client.link_group_id))
            .cloned();
        let _ = api::post_json(&client, "/dist/setClientInfo", &json!({"group_id": "", "zone": [client.zone_or_main()]}));
        if let Some(server) = server {
            let remaining: Vec<&String> = server.link_clients.iter().filter(|c| c.as_str() != ip).collect();
            let body = json!({"group_id": server.link_group_id, "zone": server.zone_or_main(), "type": "remove", "client_list": [ip]});
            let _ = api::post_json(&server, "/dist/setServerInfo", &body);
            if remaining.is_empty() {
                let _ = api::command(&server, "/dist/stopDistribution");
                let _ = api::post_json(&server, "/dist/setServerInfo", &json!({"group_id": "", "zone": server.zone_or_main()}));
            } else {
                let _ = api::command(&server, "/dist/startDistribution?num=0");
            }
        }
        // Reflect it right away; the next status round confirms.
        for d in self.devices.iter_mut() {
            if d.ip == ip {
                d.link_role = "none".into();
                d.link_group_id.clear();
            }
            d.link_clients.retain(|c| c != ip);
        }
        self.send_devices();
        self.after_link_change();
    }

    fn link_dissolve(&mut self, server: &Device) {
        for ip in &server.link_clients {
            if let Some(c) = self.device(ip) {
                let _ = api::post_json(&c, "/dist/setClientInfo", &json!({"group_id": "", "zone": [c.zone_or_main()]}));
            }
        }
        let _ = api::command(server, "/dist/stopDistribution");
        let _ = api::post_json(server, "/dist/setServerInfo", &json!({"group_id": "", "zone": server.zone_or_main()}));
        for d in self.devices.iter_mut() {
            if d.ip == server.ip || server.link_clients.contains(&d.ip) {
                d.link_role = "none".into();
                d.link_group_id.clear();
                d.link_clients.clear();
            }
        }
        self.send_devices();
    }

    fn cmd(&mut self, path: &str) {
        let Some(dev) = self.sel().cloned() else { return };
        match api::command(&dev, path) {
            Err(e) => self.notice(format!("{} ({e})", self.t("Command failed", "Opdracht mislukt"))),
            Ok(()) if path.contains("storePreset") => {
                self.notice(self.t("Preset saved", "Favoriet opgeslagen"));
                self.refresh_presets();
            }
            Ok(()) => {}
        }
        // Let the device settle, then refresh (events usually beat this).
        self.schedule_poll(Duration::from_millis(350));
    }

    fn refresh_presets(&mut self) {
        let Some(mut dev) = self.sel().cloned() else { return };
        if let Some(v) = api::get_json(&dev, "/netusb/getPresetInfo") {
            dev.presets = api::parse_presets(&v, "");
        }
        let mut tuner_presets = Vec::new();
        if dev.tuner_presets_common() {
            // One shared list; every entry reports its own band.
            if let Some(v) = api::get_json(&dev, "/tuner/getPresetInfo?band=common") {
                tuner_presets = api::parse_presets(&v, "");
            }
        } else {
            for band in dev.bands.clone() {
                if let Some(v) = api::get_json(&dev, &format!("/tuner/getPresetInfo?band={band}")) {
                    tuner_presets.extend(api::parse_presets(&v, &band));
                }
            }
        }
        dev.tuner_presets = tuner_presets;
        if let Some(d) = self.sel_mut() {
            d.presets = dev.presets;
            d.tuner_presets = dev.tuner_presets;
        }
        self.send_devices();
    }

    /// Repeat and shuffle can only be toggled; step through the device's
    /// cycle of available modes until the target is reached.
    fn set_mode(&mut self, repeat: bool, target: &str) {
        let Some(dev) = self.sel().cloned() else { return };
        let (modes, current, path) = if repeat {
            (&dev.repeat_modes, dev.repeat.as_str(), "/netusb/toggleRepeat")
        } else {
            (&dev.shuffle_modes, dev.shuffle.as_str(), "/netusb/toggleShuffle")
        };
        let (Some(from), Some(to)) =
            (modes.iter().position(|m| m == current), modes.iter().position(|m| m == target))
        else {
            return;
        };
        let steps = (to + modes.len() - from) % modes.len();
        for i in 0..steps {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(150));
            }
            if let Err(e) = api::command(&dev, path) {
                let what = if repeat {
                    self.t("Could not change repeat mode", "Herhaalstand wijzigen mislukt")
                } else {
                    self.t("Could not change shuffle mode", "Willekeurige volgorde wijzigen mislukt")
                };
                self.notice(format!("{what} ({e})"));
                break;
            }
        }
        if let Some(d) = self.sel_mut() {
            if repeat {
                d.repeat = target.to_string();
            } else {
                d.shuffle = target.to_string();
            }
        }
        self.send_devices();
        self.schedule_poll(Duration::from_millis(400));
    }

    fn play_paths(&mut self, paths: Vec<PathBuf>) {
        let files: Vec<PathBuf> = paths.into_iter().filter(|p| stream::is_supported(p)).collect();
        if files.is_empty() {
            self.notice(self.t(
                "No supported music files (mp3, flac, wav, m4a, …)",
                "Geen ondersteunde muziekbestanden (mp3, flac, wav, m4a, …)",
            ));
            return;
        }
        if self.server.is_none() {
            self.server = stream::FileServer::start();
            if self.server.is_none() {
                self.notice(self.t("Could not start the local file server", "Lokale bestandsserver starten mislukt"));
                return;
            }
        }
        if let Some(server) = &self.server {
            server.set_files(files.clone());
        }
        self.local_files = files;
        self.local_index = 0;
        self.push_local();
    }

    fn send_local_files(&self) {
        let names = self
            .local_files
            .iter()
            .map(|p| p.file_stem().and_then(|n| n.to_str()).unwrap_or("?").to_string())
            .collect();
        let current = self.local_active.then_some(self.local_index);
        self.tx.send(Event::LocalFiles { names, current });
    }

    fn local_step(&mut self, delta: i64) {
        if self.local_files.is_empty() {
            return;
        }
        let n = self.local_files.len() as i64;
        self.local_index = (self.local_index as i64 + delta).clamp(0, n - 1) as usize;
        self.push_local();
    }

    /// Auto-advance: when a pushed file was playing and the renderer stops, play the next.
    fn track_local_playback(&mut self, dev: &Device) {
        if !self.local_active {
            return;
        }
        if !dev.alive || !dev.on_dlna() {
            // Another source took over.
            self.local_active = false;
            self.send_local_files();
            return;
        }
        if dev.playing {
            self.local_saw_play = true;
        } else if dev.stopped && self.local_saw_play {
            if self.local_index + 1 < self.local_files.len() {
                self.local_index += 1;
                self.push_local();
            } else {
                self.local_active = false;
                self.send_local_files();
            }
        }
    }

    fn push_local(&mut self) {
        let Some(dev) = self.sel().cloned() else {
            self.notice(self.t("Select a speaker first", "Selecteer eerst een speaker"));
            return;
        };
        let Some(path) = self.local_files.get(self.local_index).cloned() else { return };
        let Some(server) = self.server.as_ref() else { return };
        let url = server.url_for(self.local_index, &path, &dev.ip);
        let title = path.file_stem().and_then(|n| n.to_str()).unwrap_or("Unknown").to_string();
        let mime = stream::mime_for(&path);
        if !dev.power_on {
            let _ = api::command(&dev, &dev.zpath("setPower?power=on"));
            std::thread::sleep(Duration::from_millis(1200));
        }
        // Switch to the DLNA ("server") input, then push.
        if !dev.dlna_input.is_empty() && dev.input != dev.dlna_input {
            let _ = api::command(&dev, &dev.zpath(&format!("setInput?input={}", dev.dlna_input)));
            std::thread::sleep(Duration::from_millis(600));
        }
        match stream::dlna_push(&dev.dlna_url, &url, &title, mime) {
            Ok(()) => {
                self.local_active = true;
                self.local_saw_play = false;
                self.notice(format!("{}: {title}", self.t("Streaming", "Afspelen")));
                record_play(HistoryRow {
                    title,
                    artist: String::new(),
                    album: String::new(),
                    duration: 0,
                    mediauri: url,
                    play_count: 1,
                    last_update: now_string(),
                    speaker_name: dev.display_name(),
                    speaker_ip: dev.ip.clone(),
                });
                self.schedule_poll(Duration::from_millis(800));
            }
            Err(e) => {
                self.local_active = false;
                self.notice(format!("{}: {e}", self.t("DLNA playback failed", "DLNA afspelen mislukt")));
            }
        }
        self.send_local_files();
    }

    fn browse(&mut self, input: &str, index: i64) {
        let Some(dev) = self.sel().cloned() else { return };
        self.browse_input = input.to_string();
        let path = format!("/netusb/getListInfo?input={input}&index={}&size=8&lang=en", index.max(0));
        match api::get_json(&dev, &path) {
            Some(v) => self.tx.send(Event::Browse { page: api::parse_list(&v), input: input.to_string() }),
            None => self.tx.send(Event::BrowseError(
                self.t("This source cannot be browsed.", "Deze bron kan niet worden doorbladerd.").into(),
            )),
        }
    }

    /// setListControl select/play/return. After select or return the list
    /// starts again at the top of the new level.
    fn browse_control(&mut self, kind: &str, index: i64) {
        let Some(dev) = self.sel().cloned() else { return };
        let zone = dev.zone_or_main().to_string();
        // zone is only valid for type=play; sending it with select/return
        // makes some devices answer "invalid parameter".
        let url = match kind {
            "return" => "/netusb/setListControl?list_id=main&type=return".to_string(),
            "play" => format!("/netusb/setListControl?list_id=main&type=play&index={index}&zone={zone}"),
            _ => format!("/netusb/setListControl?list_id=main&type={kind}&index={index}"),
        };
        if let Err(e) = api::command(&dev, &url) {
            self.tx.send(Event::BrowseError(format!("{} ({e})", self.t("Command failed", "Opdracht mislukt"))));
            return;
        }
        if kind == "play" {
            self.schedule_poll(Duration::from_millis(500));
            return;
        }
        std::thread::sleep(Duration::from_millis(250));
        let input = self.browse_input.clone();
        self.browse(&input, 0);
    }
}

fn clear_track(dev: &mut Device) {
    dev.track.clear();
    dev.artist.clear();
    dev.album.clear();
    dev.art_url.clear();
    dev.play_time = 0;
    dev.total_time = 0;
    dev.playing = false;
    dev.paused = false;
    dev.stopped = true;
}
