//! Responsive MusicCast controller UI.

use crate::model::*;
use crate::worker::{Event, Job};
use eframe::egui;
use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Show the standard macOS About panel — the same panel the App menu opens — so
/// the window title-bar button and the menu item share one dialog.
#[cfg(target_os = "macos")]
fn open_about() {
    use std::ffi::{c_char, c_void};

    #[link(name = "objc")]
    extern "C" {
        fn objc_getClass(name: *const c_char) -> *mut c_void;
        fn sel_registerName(name: *const c_char) -> *mut c_void;
        #[link_name = "objc_msgSend"] fn send(obj: *mut c_void, sel: *mut c_void, ...) -> *mut c_void;
    }

    // SAFETY: NSApplication is a singleton and the panel call takes a nil sender;
    // both selectors are fixed framework methods, so the message shapes are stable.
    unsafe {
        let app = send(
            objc_getClass(c"NSApplication".as_ptr()),
            sel_registerName(c"sharedApplication".as_ptr()),
        );
        if !app.is_null() {
            send(
                app,
                sel_registerName(c"orderFrontStandardAboutPanel:".as_ptr()),
                std::ptr::null_mut::<c_void>(),
            );
        }
    }
}

pub fn install_fonts(ctx: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let mut style = (*ctx.style_of(theme)).clone();
        style.spacing.item_spacing = egui::vec2(6.0, 6.0);
        style.spacing.button_padding = egui::vec2(8.0, 4.0);
        ctx.set_style_of(theme, style);
    }
}

pub struct BrowseState {
    pub entries: Vec<ListEntry>,
    pub base: i64,
    pub input: String,
}

pub struct IpDialog {
    pub ip: String,
    pub result: String,
    pub testing: bool,
}

pub struct Shared {
    pub devices: Vec<Device>,
    pub selected: String,
    pub searching: bool,
    pub searched: bool,
    pub notice: Option<(String, Instant)>,
    pub local_files: Vec<String>,
    pub browse: Option<BrowseState>,
    pub poll_secs: u32,
    pub language: Language,
    pub appearance: Appearance,
    pub ip_dialog: Option<IpDialog>,
    pub preset_num: Option<i64>,
    pub tuner_preset: Option<(String, i64)>,
    pub player_open: bool,
    pub textures: HashMap<String, egui::TextureHandle>,
    pub art_pending: Vec<(String, Vec<u8>)>,
    pub ui_volume: Option<i64>,
    pub ui_freq: Option<i64>,
    pub ui_tones: HashMap<String, i64>,
    pub tx: Sender<Job>,
}

impl Shared {
    pub fn new(tx: Sender<Job>) -> Self {
        let cfg = Config::load();
        Self {
            devices: Vec::new(), selected: String::new(), searching: false, searched: false,
            notice: None, local_files: Vec::new(), browse: None,
            poll_secs: cfg.update_seconds.clamp(1, 20), language: cfg.language,
            appearance: cfg.appearance, ip_dialog: None, preset_num: None,
            tuner_preset: None, player_open: false, textures: HashMap::new(),
            art_pending: Vec::new(), ui_volume: None, ui_freq: None,
            ui_tones: HashMap::new(), tx,
        }
    }
    pub fn send(&self, job: Job) { let _ = self.tx.send(job); }
    pub fn dev(&self) -> Option<&Device> { self.devices.iter().find(|d| d.ip == self.selected) }
    pub fn dev_mut(&mut self) -> Option<&mut Device> {
        let ip = self.selected.clone();
        self.devices.iter_mut().find(|d| d.ip == ip)
    }
    pub fn note(&mut self, msg: impl Into<String>) { self.notice = Some((msg.into(), Instant::now())); }
}

pub type SharedArc = Arc<Mutex<Shared>>;

pub struct App {
    pub shared: SharedArc,
    rx: Receiver<Event>,
    frames: u64,
    screenshot: Option<std::path::PathBuf>,
    shot_done: Arc<std::sync::atomic::AtomicBool>,
    started: bool,
    settings_open: bool,
    save_confirmation: Option<SavePreset>,
}

#[derive(Clone)]
enum SavePreset { Network(String, i64), Tuner(String, String, i64) }

impl App {
    pub fn new(shared: SharedArc, rx: Receiver<Event>, screenshot: Option<std::path::PathBuf>) -> Self {
        Self { shared, rx, frames: 0, screenshot, shot_done: Arc::new(std::sync::atomic::AtomicBool::new(false)), started: false, settings_open: false, save_confirmation: None }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        while let Ok(ev) = self.rx.try_recv() {
            let mut sh = self.shared.lock().unwrap();
            match ev {
                Event::Devices(devs) => sh.devices = devs,
                Event::Selected(ip) => {
                    if sh.selected != ip {
                        sh.preset_num = None;
                        sh.tuner_preset = None;
                        sh.ui_volume = None;
                        sh.ui_freq = None;
                        sh.ui_tones.clear();
                        sh.browse = None;
                    }
                    sh.selected = ip;
                }
                Event::Art { key, bytes } => sh.art_pending.push((key, bytes)),
                Event::Notice(msg) => sh.note(msg),
                Event::LocalFiles(files) => sh.local_files = files,
                Event::Browse { entries, base, input, .. } => sh.browse = Some(BrowseState { entries, base, input }),
                Event::Searching(on) => { sh.searching = on; if !on { sh.searched = true; } }
                Event::IpTestResult(msg) => if let Some(d) = &mut sh.ip_dialog { d.result = msg; d.testing = false; },
            }
        }
        let mut sh = self.shared.lock().unwrap();
        for (key, bytes) in std::mem::take(&mut sh.art_pending) {
            if !sh.textures.contains_key(&key) {
                if let Ok(img) = image::load_from_memory(&bytes) {
                    let rgba = img.to_rgba8();
                    let color = egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                    sh.textures.insert(key.clone(), ctx.load_texture(key, color, egui::TextureOptions::LINEAR));
                }
            }
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        self.drain_events(&ctx);
        if !self.started {
            self.started = true;
            let sh = self.shared.lock().unwrap();
            sh.send(Job::LoadSaved);
            let tx = sh.tx.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_secs(1));
                let _ = tx.send(Job::Discover);
            });
        }
        ctx.request_repaint_after(Duration::from_millis(100));
        let (lang, appearance) = { let sh = self.shared.lock().unwrap(); (sh.language, sh.appearance) };
        ctx.set_theme(match appearance {
            Appearance::System => egui::ThemePreference::System,
            Appearance::Light => egui::ThemePreference::Light,
            Appearance::Dark => egui::ThemePreference::Dark,
        });

        egui::CentralPanel::default().show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.heading("MusicCast");
                ui.separator();
                ui.label(lang.text("Speaker controller", "Speakerbediening"));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(lang.text("About", "Over")).clicked() { show_about(lang); }
                    if ui.button(lang.text("Settings", "Instellingen")).clicked() { self.settings_open = true; }
                });
            });
            ui.separator();
            let notice = self.shared.lock().unwrap().notice.clone();
            if let Some((message, time)) = notice {
                if time.elapsed() < Duration::from_secs(8) { ui.label(message); }
            }
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                if ui.available_width() >= 850.0 {
                    ui.columns(3, |columns| {
                        devices_panel(&mut columns[0], &self.shared, lang);
                        playback_panel(&mut columns[1], &self.shared, lang);
                        details_panel(&mut columns[2], &self.shared, lang, &mut self.save_confirmation);
                    });
                } else {
                    devices_panel(ui, &self.shared, lang);
                    playback_panel(ui, &self.shared, lang);
                    details_panel(ui, &self.shared, lang, &mut self.save_confirmation);
                }
            });
        });

        settings_window(&ctx, &self.shared, &mut self.settings_open, lang);
        ip_dialog_window(&ctx, &self.shared, lang);
        browse_window(&ctx, &self.shared, lang);
        save_dialog(&ctx, &self.shared, &mut self.save_confirmation, lang);

        if self.shared.lock().unwrap().player_open {
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                egui::ViewportId::from_hash_of("player"),
                egui::ViewportBuilder::default().with_title(lang.text("Player", "Speler")).with_inner_size([440.0, 480.0]).with_min_inner_size([300.0, 300.0]),
                move |ui, _| player_ui(ui, &shared),
            );
        }

        if self.screenshot.is_some() {
            self.frames += 1;
            if self.frames == 120 {
                let path = self.screenshot.clone().unwrap();
                let done = self.shot_done.clone();
                ctx.request_screenshot(move |img| {
                    let rgba = image::RgbaImage::from_fn(img.width() as u32, img.height() as u32, |x, y| image::Rgba(img.pixels[(y as usize) * img.width() + (x as usize)].to_array()));
                    if let Err(e) = rgba.save(path) { eprintln!("screenshot save failed: {e}"); }
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if self.shot_done.load(std::sync::atomic::Ordering::SeqCst) { ctx.send_viewport_cmd(egui::ViewportCommand::Close); }
        }
    }
}

fn section(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.add(egui::Label::new(egui::RichText::new(title).size(16.0).strong()));
        ui.add_space(2.0);
        add(ui);
    });
    ui.add_space(6.0);
}

fn send(shared: &SharedArc, job: Job) { shared.lock().unwrap().send(job); }
fn open_ip(shared: &SharedArc) {
    shared.lock().unwrap().ip_dialog = Some(IpDialog { ip: String::new(), result: String::new(), testing: false });
}
fn status(dev: &Device, lang: Language) -> &'static str {
    if !dev.alive { lang.text("Offline", "Offline") }
    else if dev.power_on { lang.text("On", "Aan") }
    else { lang.text("Standby", "Stand-by") }
}

fn devices_panel(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let (devices, selected, searching, searched) = {
        let sh = shared.lock().unwrap();
        (sh.devices.clone(), sh.selected.clone(), sh.searching, sh.searched)
    };
    section(ui, lang.text("Speakers", "Speakers"), |ui| {
        if searching || !searched {
            ui.horizontal(|ui| { ui.spinner(); ui.label(lang.text("Searching for speakers…", "Speakers zoeken…")); });
        } else if devices.is_empty() {
            ui.label(lang.text("No speakers found. Make sure your speaker is on the same network.", "Geen speakers gevonden. Controleer of je speaker op hetzelfde netwerk zit."));
        }
        for dev in &devices {
            let text = format!("{}\n{} · {} · {}", dev.display_name(), status(dev, lang), dev.model, dev.ip);
            if ui.selectable_label(dev.ip == selected, text).on_hover_text(lang.text("Select speaker", "Speaker selecteren")).clicked() {
                send(shared, Job::Select(dev.ip.clone()));
            }
        }
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!searching, egui::Button::new(lang.text("Search again", "Opnieuw zoeken"))).on_hover_text(lang.text("Search the local network", "Zoek op het lokale netwerk")).clicked() { send(shared, Job::Discover); }
            if ui.button(lang.text("Add by IP", "Toevoegen via IP")).clicked() { open_ip(shared); }
            if ui.add_enabled(!selected.is_empty(), egui::Button::new(lang.text("Remove", "Verwijderen"))).on_hover_text(lang.text("Remove selected speaker", "Geselecteerde speaker verwijderen")).clicked() { send(shared, Job::Remove(selected.clone())); }
        });
    });
    let dev = shared.lock().unwrap().dev().cloned();
    section(ui, lang.text("Input & zone", "Ingang en zone"), |ui| {
        if let Some(dev) = dev {
            ui.add_enabled_ui(dev.alive, |ui| {
                if !dev.zones.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(lang.text("Zone", "Zone"));
                        egui::ComboBox::from_id_salt("zone").selected_text(&dev.zone).show_ui(ui, |ui| {
                            for zone in &dev.zones {
                                if ui.selectable_label(zone == &dev.zone, zone).clicked() { send(shared, Job::SetZone(zone.clone())); }
                            }
                        });
                    });
                }
                if dev.inputs.is_empty() { ui.label(lang.text("No inputs available.", "Geen ingangen beschikbaar.")); }
                else {
                    egui::ScrollArea::vertical().id_salt("inputs").max_height(180.0).show(ui, |ui| {
                        for input in &dev.inputs {
                            if ui.selectable_label(input.id == dev.input, &input.title).clicked() {
                                let mut sh = shared.lock().unwrap();
                                if let Some(d) = sh.dev_mut() { d.input = input.id.clone(); }
                                sh.send(Job::Cmd(format!("/main/setInput?input={}", input.id)));
                            }
                        }
                    });
                }
            });
        } else { ui.label(lang.text("Select a speaker to see its inputs.", "Selecteer een speaker om de ingangen te zien.")); }
    });
}

fn playback_panel(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let dev = shared.lock().unwrap().dev().cloned();
    section(ui, lang.text("Now playing", "Nu aan het spelen"), |ui| {
        let Some(d) = dev else { ui.label(lang.text("Select a speaker to control playback.", "Selecteer een speaker om de weergave te bedienen.")); return; };
        ui.strong(d.display_name());
        ui.label(status(&d, lang));
        ui.add_space(4.0);
        let tex = shared.lock().unwrap().textures.get(&d.art_url).cloned();
        if let Some(tex) = tex {
            let side = ui.available_width().min(132.0);
            ui.add(egui::Image::new((tex.id(), egui::vec2(side, side))));
        } else {
            ui.label(lang.text("No album artwork", "Geen albumhoes"));
        }
        if !d.track.is_empty() { ui.strong(&d.track); }
        else if d.input == "tuner" { ui.strong(lang.text("Radio", "Radio")); }
        else { ui.strong(lang.text("Nothing playing", "Niets aan het spelen")); }
        if !d.artist.is_empty() { ui.label(&d.artist); }
        if !d.album.is_empty() { ui.label(&d.album); }
        if !d.input.is_empty() { ui.small(format!("{}: {}", lang.text("Input", "Ingang"), d.input)); }
        if !d.au_format.is_empty() { ui.small(format!("{} {}", d.au_format, d.au_fs)); }
        ui.add_enabled_ui(d.alive, |ui| {
            ui.horizontal_wrapped(|ui| {
                if ui.button(if d.power_on { lang.text("Turn off", "Uitschakelen") } else { lang.text("Turn on", "Inschakelen") }).clicked() {
                    send(shared, Job::Cmd("/main/setPower?power=toggle".into()));
                }
                if ui.button(if d.muted { lang.text("Unmute", "Geluid aan") } else { lang.text("Mute", "Dempen") }).clicked() {
                    send(shared, Job::Cmd(format!("/main/setMute?enable={}", !d.muted)));
                }
                if ui.button(lang.text("Open player", "Speler openen")).clicked() { shared.lock().unwrap().player_open = true; }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button(lang.text("Previous", "Vorige")).clicked() { send(shared, Job::Cmd("/netusb/setPlayback?playback=previous".into())); }
                if ui.button(if d.playing { lang.text("Pause", "Pauzeren") } else { lang.text("Play", "Afspelen") }).clicked() {
                    send(shared, Job::Cmd(format!("/netusb/setPlayback?playback={}", if d.playing { "pause" } else { "play" })));
                }
                if ui.button(lang.text("Next", "Volgende")).clicked() { send(shared, Job::Cmd("/netusb/setPlayback?playback=next".into())); }
            });
            volume_control(ui, shared, &d, lang);
            ui.horizontal_wrapped(|ui| {
                if ui.button(lang.text("Browse", "Bladeren")).clicked() { send(shared, Job::Browse { input: d.input.clone(), index: 0 }); }
                if ui.button(lang.text("Open file…", "Bestand openen…")).clicked() {
                    if let Some(path) = rfd::FileDialog::new().pick_file() { send(shared, Job::PlayPaths(vec![path])); }
                }
            });
            ui.separator();
            ui.horizontal_wrapped(|ui| {
                ui.label(lang.text("Repeat:", "Herhalen:"));
                for (value, text) in [("off", lang.text("Off", "Uit")), ("one", lang.text("One", "Eén")), ("all", lang.text("All", "Alles"))] {
                    if ui.add_enabled(d.repeat_available, egui::Button::selectable(d.repeat == value, text)).clicked() { send(shared, Job::SetRepeat(value.into())); }
                }
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(lang.text("Shuffle:", "Willekeurig:"));
                for (value, text) in [(false, lang.text("Off", "Uit")), (true, lang.text("On", "Aan"))] {
                    if ui.add_enabled(d.shuffle_available, egui::Button::selectable(d.shuffle == value, text)).clicked() { send(shared, Job::SetShuffle(value)); }
                }
            });
        });
    });
}

fn volume_control(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    let mut volume = shared.lock().unwrap().ui_volume.unwrap_or(d.volume);
    let min = d.volume_min;
    let max = d.volume_max.max(min + 1);
    let response = ui.add(egui::Slider::new(&mut volume, min..=max).text(lang.text("Volume", "Volume")));
    if response.changed() || response.drag_stopped() {
        let mut sh = shared.lock().unwrap();
        if response.dragged() { sh.ui_volume = Some(volume); }
        if response.drag_stopped() || (response.changed() && !response.dragged()) {
            let volume = step_val(volume, d.volume_step.max(1)).clamp(min, max);
            sh.ui_volume = None;
            if let Some(dev) = sh.dev_mut() { dev.volume = volume; }
            sh.send(Job::Cmd(format!("/main/setVolume?volume={volume}")));
        }
    }
}

fn preset_label(p: &Preset, index: usize, lang: Language) -> String {
    if p.text.trim().is_empty() || p.text.trim().eq_ignore_ascii_case("unknown") || p.band.trim().eq_ignore_ascii_case("unknown") {
        format!("{}. {}", index + 1, lang.text("Empty slot", "Lege plek"))
    } else { p.label(index) }
}
fn preset_empty(p: &Preset) -> bool {
    p.text.trim().is_empty() || p.text.trim().eq_ignore_ascii_case("unknown") || p.band.trim().eq_ignore_ascii_case("unknown")
}

fn details_panel(ui: &mut egui::Ui, shared: &SharedArc, lang: Language, confirmation: &mut Option<SavePreset>) {
    let dev = shared.lock().unwrap().dev().cloned();
    let Some(d) = dev else {
        section(ui, lang.text("Presets & sound", "Favorieten en geluid"), |ui| { ui.label(lang.text("Select a speaker to view its settings.", "Selecteer een speaker om de instellingen te bekijken.")); });
        return;
    };
    ui.add_enabled_ui(d.alive, |ui| {
        section(ui, lang.text("Presets", "Favorieten"), |ui| {
            let selected = shared.lock().unwrap().preset_num;
            if d.presets.is_empty() { ui.label(lang.text("No presets available.", "Geen favorieten beschikbaar.")); }
            else {
                let current = d.presets.iter().enumerate().find(|(_, p)| Some(p.num) == selected).map(|(i,p)| preset_label(p,i,lang)).unwrap_or_else(|| lang.text("Select a preset", "Kies een favoriet").into());
                egui::ComboBox::from_id_salt("preset").selected_text(current).width(ui.available_width().max(80.0)).show_ui(ui, |ui| {
                    for (i,p) in d.presets.iter().enumerate() {
                        if ui.selectable_label(Some(p.num) == selected, preset_label(p,i,lang)).clicked() {
                            let mut sh = shared.lock().unwrap();
                            sh.preset_num = Some(p.num);

                        }
                    }
                });
                let selected_preset = d.presets.iter().find(|p| Some(p.num) == selected);
                if ui.add_enabled(selected_preset.is_some_and(|p| !preset_empty(p)), egui::Button::new(lang.text("Recall preset", "Favoriet afspelen"))).clicked() {
                    if let Some(p) = selected_preset { send(shared, Job::Cmd(format!("/netusb/recallPreset?zone=main&num={}", p.num))); }
                }
                if ui.add_enabled(selected.is_some(), egui::Button::new(lang.text("Save current to selected slot…", "Huidige opslaan op gekozen plek…"))).clicked() {
                    *confirmation = selected.map(|num| SavePreset::Network(d.ip.clone(), num));
                }
            }
        });
        if d.has_tuner() && d.input == "tuner" {
            section(ui, lang.text("Radio", "Radio"), |ui| {
                let selected = shared.lock().unwrap().tuner_preset.clone();
                if d.tuner_presets.is_empty() { ui.label(lang.text("No radio presets available.", "Geen radiofavorieten beschikbaar.")); }
                else {
                    let current = d.tuner_presets.iter().enumerate().find(|(_,p)| selected.as_ref() == Some(&(p.band.clone(),p.num))).map(|(i,p)| preset_label(p,i,lang)).unwrap_or_else(|| lang.text("Select a radio preset", "Kies een radiofavoriet").into());
                    egui::ComboBox::from_id_salt("tuner_preset").selected_text(current).width(ui.available_width().max(80.0)).show_ui(ui, |ui| {
                        for (i,p) in d.tuner_presets.iter().enumerate() {
                            if ui.selectable_label(selected.as_ref() == Some(&(p.band.clone(),p.num)), preset_label(p,i,lang)).clicked() {
                                let mut sh = shared.lock().unwrap();
                                sh.tuner_preset = Some((p.band.clone(),p.num));

                            }
                        }
                    });
                    let selected_preset = d.tuner_presets.iter().find(|p| selected.as_ref() == Some(&(p.band.clone(), p.num)));
                    if ui.add_enabled(selected_preset.is_some_and(|p| !preset_empty(p)), egui::Button::new(lang.text("Recall station", "Zender afspelen"))).clicked() {
                        if let Some(p) = selected_preset { send(shared, Job::Cmd(format!("/tuner/recallPreset?zone=main&band={}&num={}", p.band, p.num))); }
                    }
                    if ui.add_enabled(selected.is_some(), egui::Button::new(lang.text("Save station to selected slot…", "Zender opslaan op gekozen plek…"))).clicked() {
                        *confirmation = selected.map(|(band, num)| SavePreset::Tuner(d.ip.clone(), band, num));
                    }
                }
                ui.horizontal_wrapped(|ui| {
                    ui.label(lang.text("Band", "Band"));
                    egui::ComboBox::from_id_salt("band").selected_text(&d.band).show_ui(ui, |ui| {
                        for band in &d.bands {
                            if ui.selectable_label(band == &d.band, band).clicked() { send(shared, Job::Cmd(format!("/tuner/setBand?band={band}"))); }
                        }
                    });
                });
                let range = d.current_band_range();
                if range.max > range.min {
                    let mut freq = shared.lock().unwrap().ui_freq.unwrap_or(d.tuner_freq).clamp(range.min, range.max);
                    let text = if d.band.to_lowercase().contains("fm") { format!("{:.1} MHz", freq as f64 / 1000.0) } else { format!("{freq} kHz") };
                    let response = ui.add(egui::Slider::new(&mut freq, range.min..=range.max).show_value(false).text(text));
                    if response.changed() || response.drag_stopped() {
                        let mut sh = shared.lock().unwrap();
                        if response.dragged() { sh.ui_freq = Some(freq); }
                        if response.drag_stopped() || (response.changed() && !response.dragged()) {
                            let freq = step_val(freq, range.step.max(1)).clamp(range.min, range.max);
                            sh.ui_freq = None;
                            if let Some(dev) = sh.dev_mut() { dev.tuner_freq = freq; }
                            sh.send(Job::Cmd(format!("/tuner/setFreq?band={}&tuning=direct&num={freq}", d.band)));
                        }
                    }
                }
            });
        }
        section(ui, lang.text("Sound", "Geluid"), |ui| {
            if !d.programs.is_empty() {
                ui.label(lang.text("Sound program", "Geluidsprogramma"));
                egui::ComboBox::from_id_salt("effects").selected_text(&d.sound_program).width(ui.available_width().max(80.0)).show_ui(ui, |ui| {
                    for program in &d.programs {
                        if ui.selectable_label(program == &d.sound_program, program).clicked() { send(shared, Job::Cmd(format!("/main/setSoundProgram?program={program}"))); }
                    }
                });
            }
            for (name, feature, title, on) in [
                ("Enhancer", "enhancer", lang.text("Enhancer", "Verbetering"), d.enhancer),
                ("Direct", "direct", "Direct", d.direct),
                ("BassExtension", "bass_extension", lang.text("Bass extension", "Basversterking"), d.bass_extension),
                ("ClearVoice", "clear_voice", lang.text("Clear voice", "Heldere stem"), d.clear_voice),
            ] {
                if !d.func_list.iter().any(|f| f == feature) { continue; }
                let mut checked = on;
                if ui.checkbox(&mut checked, title).changed() { send(shared, Job::Cmd(format!("/main/set{name}?enable={checked}"))); }
            }
            if d.tones.is_empty() { ui.label(lang.text("No tone controls available.", "Geen toonregeling beschikbaar.")); }
            for tone in &d.tones {
                let min = d.tone_min;
                let max = d.tone_max.max(min + 1);
                let mut value = shared.lock().unwrap().ui_tones.get(&tone.name).copied().unwrap_or(tone.value);
                let name = match tone.name.as_str() { "bass" => lang.text("Bass", "Bas"), "treble" => lang.text("Treble", "Hoge tonen"), _ => &tone.name };
                let response = ui.add(egui::Slider::new(&mut value, min..=max).text(name));
                if response.changed() || response.drag_stopped() {
                    let mut sh = shared.lock().unwrap();
                    if response.dragged() { sh.ui_tones.insert(tone.name.clone(), value); }
                    if response.drag_stopped() || (response.changed() && !response.dragged()) {
                        let value = step_val(value, d.tone_step.max(1)).clamp(min, max);
                        sh.ui_tones.remove(&tone.name);
                        if let Some(dev) = sh.dev_mut() { if let Some(t) = dev.tones.iter_mut().find(|t| t.name == tone.name) { t.value = value; } }
                        sh.send(Job::Cmd(format!("/main/setToneControl?mode=manual&{}={value}", tone.name)));
                    }
                }
            }
            if d.sys_func_list.iter().any(|f| f == "dimmer") && ui.button(format!("{}: {}", lang.text("Display dimmer", "Displaydimmer"), d.dimmer)).clicked() {
                let next = if d.dimmer + d.dim_step.max(1) > d.dim_max { d.dim_min } else { d.dimmer + d.dim_step.max(1) };
                send(shared, Job::Cmd(format!("/system/setDimmer?value={next}")));
            }
        });
    });
    let files = shared.lock().unwrap().local_files.clone();
    section(ui, lang.text("Local files", "Lokale bestanden"), |ui| {
        if files.is_empty() { ui.label(lang.text("Open a music file to play it here.", "Open een muziekbestand om het hier af te spelen.")); }
        let can_play = shared.lock().unwrap().dev().is_some_and(|d| d.alive);
        for (i, file) in files.iter().enumerate() {
            if ui.add_enabled(can_play, egui::Button::new(file)).clicked() { send(shared, Job::LocalJump(i)); }
        }
    });
}

fn settings_window(ctx: &egui::Context, shared: &SharedArc, open: &mut bool, lang: Language) {
    if !*open { return; }
    egui::Window::new(lang.text("Settings", "Instellingen")).id(egui::Id::unique("settings"))
        .open(open).default_width(360.0).show(ctx, |ui| {
            let (mut language, mut appearance, mut seconds) = { let sh = shared.lock().unwrap(); (sh.language, sh.appearance, sh.poll_secs) };
            ui.label(lang.text("Appearance", "Weergave"));
            egui::ComboBox::from_id_salt("appearance").selected_text(match appearance {
                Appearance::System => lang.text("Follow system", "Systeem volgen"), Appearance::Light => lang.text("Light", "Licht"), Appearance::Dark => lang.text("Dark", "Donker"),
            }).show_ui(ui, |ui| {
                ui.selectable_value(&mut appearance, Appearance::System, lang.text("Follow system", "Systeem volgen"));
                ui.selectable_value(&mut appearance, Appearance::Light, lang.text("Light", "Licht"));
                ui.selectable_value(&mut appearance, Appearance::Dark, lang.text("Dark", "Donker"));
            });
            ui.add_space(8.0);
            ui.label(lang.text("Language", "Taal"));
            egui::ComboBox::from_id_salt("language").selected_text(match language { Language::English => "English", Language::Dutch => "Nederlands" }).show_ui(ui, |ui| {
                ui.selectable_value(&mut language, Language::English, "English");
                ui.selectable_value(&mut language, Language::Dutch, "Nederlands");
            });
            ui.add_space(8.0);
            ui.label(lang.text("Speaker refresh interval (seconds)", "Verversinterval speaker (seconden)"));
            if ui.add(egui::Slider::new(&mut seconds, 1..=20)).changed() {
                let mut sh = shared.lock().unwrap();
                sh.poll_secs = seconds;
                sh.send(Job::SetPollSecs(seconds));
            }
            let mut sh = shared.lock().unwrap();
            if language != sh.language || appearance != sh.appearance {
                sh.language = language;
                sh.appearance = appearance;
                sh.send(Job::SetPreferences { language, appearance });
            }
        });
}

/// Open the app's single About dialog: the native macOS panel, or a plain
/// message box where that panel does not exist.
fn show_about(_lang: Language) {
    #[cfg(target_os = "macos")]
    open_about();
    #[cfg(not(target_os = "macos"))]
    rfd::MessageDialog::new()
        .set_title(_lang.text("About", "Over"))
        .set_description(format!(
            "Controller for MusicCast {}\n\n{}",
            env!("CARGO_PKG_VERSION"),
            _lang.text(
                "Control compatible Yamaha MusicCast speakers on your local network.",
                "Bedien compatibele Yamaha MusicCast-speakers op je lokale netwerk.",
            )
        ))
        .set_level(rfd::MessageLevel::Info)
        .show();
}

fn ip_dialog_window(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let mut dialog = match shared.lock().unwrap().ip_dialog.take() { Some(d) => d, None => return };
    let mut open = true;
    let mut close = false;
    egui::Window::new(lang.text("Add speaker", "Speaker toevoegen")).id(egui::Id::unique("add_ip"))
        .open(&mut open).collapsible(false).default_width(360.0).show(ctx, |ui| {
            ui.label(lang.text("Enter the IP address of your MusicCast speaker:", "Voer het IP-adres van je MusicCast-speaker in:"));
            ui.add(egui::TextEdit::singleline(&mut dialog.ip).hint_text("192.168.1.123"));
            if dialog.testing { ui.label(lang.text("Testing…", "Bezig met testen…")); }
            else if !dialog.result.is_empty() {
                if dialog.result == "Test OK" { ui.label(lang.text("Speaker found.", "Speaker gevonden.")); }
                else { ui.label(format!("{}: {}", lang.text("Test failed", "Test mislukt"), dialog.result)); }
            }
            let valid = dialog.ip.trim().parse::<std::net::Ipv4Addr>().is_ok();
            if !dialog.ip.trim().is_empty() && !valid { ui.label(lang.text("Enter a valid IPv4 address.", "Voer een geldig IPv4-adres in.")); }
            ui.horizontal_wrapped(|ui| {
                if ui.add_enabled(valid && !dialog.testing, egui::Button::new(lang.text("Test", "Testen"))).clicked() {
                    dialog.testing = true;
                    dialog.result.clear();
                    send(shared, Job::TestIp(dialog.ip.trim().to_string()));
                }
                if ui.add_enabled(valid && !dialog.testing, egui::Button::new(lang.text("Add", "Toevoegen"))).clicked() {
                    send(shared, Job::AddManual(dialog.ip.trim().to_string()));
                    close = true;
                }
                if ui.button(lang.text("Cancel", "Annuleren")).clicked() { close = true; }
            });
        });
    if open && !close { shared.lock().unwrap().ip_dialog = Some(dialog); }
}

fn browse_window(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let data = { let sh = shared.lock().unwrap(); sh.browse.as_ref().map(|b| (b.entries.clone(), b.base, b.input.clone())) };
    let Some((entries, base, input)) = data else { return; };
    let mut open = true;
    egui::Window::new(lang.text("Browse", "Bladeren")).id(egui::Id::unique("browse"))
        .open(&mut open).default_width(420.0).show(ctx, |ui| {
            ui.label(format!("{}: {}", lang.text("Input", "Ingang"), input));
            if entries.is_empty() { ui.label(lang.text("This list is empty.", "Deze lijst is leeg.")); }
            egui::ScrollArea::vertical().max_height(320.0).show(ui, |ui| {
                for (i, entry) in entries.iter().enumerate() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(&entry.text);
                        if ui.button(lang.text("Open", "Openen")).clicked() { send(shared, Job::BrowseSelect(base + i as i64)); }
                        if ui.button(lang.text("Play", "Afspelen")).clicked() { send(shared, Job::BrowsePlay(base + i as i64)); }
                    });
                }
            });
            ui.horizontal_wrapped(|ui| {
                if ui.button(lang.text("Back", "Terug")).clicked() { send(shared, Job::BrowseReturn); }
                if ui.button(lang.text("Refresh", "Vernieuwen")).clicked() { send(shared, Job::Browse { input: input.clone(), index: base }); }
            });
        });
    if !open { shared.lock().unwrap().browse = None; }
}

fn save_dialog(ctx: &egui::Context, shared: &SharedArc, confirmation: &mut Option<SavePreset>, lang: Language) {
    let Some(choice) = confirmation.clone() else { return; };
    let mut open = true;
    let mut close = false;
    let (speaker, band, num) = match &choice { SavePreset::Network(ip, num) => (ip.as_str(), None, *num), SavePreset::Tuner(ip, band, num) => (ip.as_str(), Some(band.as_str()), *num) };
    egui::Window::new(lang.text("Save preset?", "Favoriet opslaan?")).id(egui::Id::unique("save_confirm"))
        .open(&mut open).collapsible(false).resizable(false).show(ctx, |ui| {
            ui.label(format!("{} {}{} {}", lang.text("Replace the contents of slot", "Inhoud vervangen van plek"), num, band.map(|b| format!(" ({b})")).unwrap_or_default(), lang.text("on the selected speaker?", "op de geselecteerde speaker?")));
            if let Some(band) = band {
                let current_band = shared.lock().unwrap().dev().map(|d| d.band.clone()).unwrap_or_default();
                if current_band != band { ui.label(lang.text("Select this radio band before saving.", "Kies eerst deze radioband voordat je opslaat.")); }
            }
            ui.horizontal(|ui| {
                let valid_speaker = shared.lock().unwrap().dev().is_some_and(|d| d.ip == speaker && d.alive && band.is_none_or(|b| d.band == b && d.input == "tuner"));
                if ui.add_enabled(valid_speaker, egui::Button::new(lang.text("Save", "Opslaan"))).clicked() {
                    match &choice {
                        SavePreset::Network(_, n) => send(shared, Job::Cmd(format!("/netusb/storePreset?num={n}"))),
                        SavePreset::Tuner(_, _, n) => send(shared, Job::Cmd(format!("/tuner/storePreset?num={n}"))),
                    }
                    close = true;
                }
                if ui.button(lang.text("Cancel", "Annuleren")).clicked() { close = true; }
            });
        });
    if !open || close { *confirmation = None; }
}

fn player_ui(ui: &mut egui::Ui, shared: &SharedArc) {
    if ui.input(|i| i.viewport().close_requested()) { shared.lock().unwrap().player_open = false; }
    let lang = shared.lock().unwrap().language;
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| { playback_panel(ui, shared, lang); });
    });
}
