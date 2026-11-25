//! Responsive MusicCast controller UI.
//!
//! Layout: a header with the selected speaker and its power button, a sidebar
//! with speakers and inputs, and a main area with "Now playing" next to presets,
//! radio and sound settings. Narrow windows stack everything in one column.

use crate::api::ListPage;
use crate::model::*;
use crate::worker::{Event, Job};
use eframe::egui::{self, Color32, RichText, Vec2};
use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const NOTICE_SECS: u64 = 8;
const MAX_ART_TEXTURES: usize = 30;
const GREEN: Color32 = Color32::from_rgb(52, 199, 89);
const ORANGE: Color32 = Color32::from_rgb(255, 159, 10);

pub fn player_viewport_id() -> egui::ViewportId {
    egui::ViewportId::from_hash_of("player")
}

/// Show the standard macOS About panel — the same panel the App menu opens — so
/// the header button and the menu item share one dialog.
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

pub fn install_style(ctx: &egui::Context) {
    for theme in [egui::Theme::Light, egui::Theme::Dark] {
        let mut style = (*ctx.style_of(theme)).clone();
        style.spacing.item_spacing = egui::vec2(8.0, 6.0);
        style.spacing.button_padding = egui::vec2(10.0, 5.0);
        style.spacing.interact_size.y = 26.0;
        ctx.set_style_of(theme, style);
    }
}

// ---------------------------------------------------------------- state

pub struct BrowseState {
    pub page: Option<ListPage>,
    pub input: String,
    pub loading: bool,
    pub error: String,
}

pub struct IpDialog {
    pub ip: String,
    pub result: Option<Result<String, String>>,
    pub testing: bool,
}

#[derive(Clone)]
pub enum Confirm {
    SavePreset { ip: String, tuner: bool, band: String, num: i64 },
    Remove { ip: String, name: String },
}

pub struct Shared {
    pub devices: Vec<Device>,
    pub selected: String,
    pub searching: bool,
    pub searched: bool,
    pub notice: Option<(String, Instant)>,
    pub local_files: Vec<String>,
    pub local_current: Option<usize>,
    pub browse: Option<BrowseState>,
    pub poll_secs: u32,
    pub language: Language,
    pub appearance: Appearance,
    pub ip_dialog: Option<IpDialog>,
    pub confirm: Option<Confirm>,
    pub player_open: bool,
    pub textures: HashMap<String, egui::TextureHandle>,
    art_order: VecDeque<String>,
    art_pending: Vec<(String, Vec<u8>)>,
    pub ui_volume: Option<i64>,
    pub ui_freq: Option<i64>,
    pub ui_tones: HashMap<String, i64>,
    /// Group member volume while its slider is dragged.
    pub ui_member_volume: HashMap<String, i64>,
    /// Last reported play_time and when it arrived, to advance the clock locally.
    play_clock: Option<(i64, Instant)>,
    scroll_acc: f32,
    pub tx: Sender<Job>,
}

impl Shared {
    pub fn new(tx: Sender<Job>) -> Self {
        let cfg = Config::load();
        Self {
            devices: Vec::new(),
            selected: String::new(),
            searching: false,
            searched: false,
            notice: None,
            local_files: Vec::new(),
            local_current: None,
            browse: None,
            poll_secs: cfg.update_seconds.clamp(1, 20),
            language: cfg.language,
            appearance: cfg.appearance,
            ip_dialog: None,
            confirm: None,
            player_open: false,
            textures: HashMap::new(),
            art_order: VecDeque::new(),
            art_pending: Vec::new(),
            ui_volume: None,
            ui_freq: None,
            ui_tones: HashMap::new(),
            ui_member_volume: HashMap::new(),
            play_clock: None,
            scroll_acc: 0.0,
            tx,
        }
    }
    pub fn send(&self, job: Job) {
        let _ = self.tx.send(job);
    }
    pub fn dev(&self) -> Option<&Device> {
        self.devices.iter().find(|d| d.ip == self.selected)
    }
    pub fn dev_mut(&mut self) -> Option<&mut Device> {
        let ip = self.selected.clone();
        self.devices.iter_mut().find(|d| d.ip == ip)
    }
    pub fn note(&mut self, msg: impl Into<String>) {
        self.notice = Some((msg.into(), Instant::now()));
    }
    /// Current position, advanced locally between status updates.
    fn play_time(&self, d: &Device) -> i64 {
        match self.play_clock {
            Some((base, at)) if d.playing => {
                let t = base + at.elapsed().as_secs() as i64;
                if d.total_time > 0 { t.min(d.total_time) } else { t }
            }
            _ => d.play_time,
        }
    }
}

pub type SharedArc = Arc<Mutex<Shared>>;

pub struct App {
    pub shared: SharedArc,
    rx: Receiver<Event>,
    frames: u64,
    screenshot: Option<std::path::PathBuf>,
    shot_done: Arc<std::sync::atomic::AtomicBool>,
    started: bool,
    demo: bool,
    settings_open: bool,
}

impl App {
    pub fn new(shared: SharedArc, rx: Receiver<Event>, screenshot: Option<std::path::PathBuf>, demo: bool) -> Self {
        Self {
            shared,
            rx,
            frames: 0,
            screenshot,
            shot_done: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            started: false,
            demo,
            settings_open: false,
        }
    }

    fn drain_events(&mut self, ctx: &egui::Context) {
        let mut sh = self.shared.lock().unwrap();
        while let Ok(ev) = self.rx.try_recv() {
            match ev {
                Event::Devices(devs) => {
                    sh.devices = devs;
                    let now = Instant::now();
                    if let Some(t) = sh.dev().map(|d| d.play_time) {
                        if sh.play_clock.is_none_or(|(base, _)| base != t) {
                            sh.play_clock = Some((t, now));
                        }
                    }
                }
                Event::Selected(ip) => {
                    if sh.selected != ip {
                        sh.ui_volume = None;
                        sh.ui_freq = None;
                        sh.ui_tones.clear();
                        sh.browse = None;
                        sh.play_clock = None;
                    }
                    sh.selected = ip;
                }
                Event::Art { key, bytes } => sh.art_pending.push((key, bytes)),
                Event::Notice(msg) => sh.note(msg),
                Event::LocalFiles { names, current } => {
                    sh.local_files = names;
                    sh.local_current = current;
                }
                Event::Browse { page, input } => {
                    if let Some(b) = &mut sh.browse {
                        *b = BrowseState { page: Some(page), input, loading: false, error: String::new() };
                    }
                }
                Event::BrowseError(msg) => match &mut sh.browse {
                    Some(b) => {
                        b.loading = false;
                        b.error = msg;
                    }
                    None => sh.note(msg),
                },
                Event::Searching(on) => {
                    sh.searching = on;
                    if !on {
                        sh.searched = true;
                    }
                }
                Event::IpTestResult(res) => {
                    if let Some(d) = &mut sh.ip_dialog {
                        d.result = Some(res);
                        d.testing = false;
                    }
                }
            }
        }
        for (key, bytes) in std::mem::take(&mut sh.art_pending) {
            if sh.textures.contains_key(&key) {
                continue;
            }
            if let Ok(img) = image::load_from_memory(&bytes) {
                // Album art is shown at most ~260 px; keep textures small.
                let img = if img.width() > 512 { img.thumbnail(512, 512) } else { img };
                let rgba = img.to_rgba8();
                let color =
                    egui::ColorImage::from_rgba_unmultiplied([rgba.width() as usize, rgba.height() as usize], rgba.as_raw());
                let tex = ctx.load_texture(key.clone(), color, egui::TextureOptions::LINEAR);
                sh.textures.insert(key.clone(), tex);
                sh.art_order.push_back(key);
            }
        }
        // Evict old album art; device icons and the current art stay.
        while sh.art_order.len() > MAX_ART_TEXTURES {
            let keep: Vec<String> =
                sh.devices.iter().flat_map(|d| [d.icon_url.clone(), d.art_url.clone()]).collect();
            let Some(pos) = sh.art_order.iter().position(|k| !keep.contains(k)) else { break };
            if let Some(key) = sh.art_order.remove(pos) {
                sh.textures.remove(&key);
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
            if !self.demo {
                let sh = self.shared.lock().unwrap();
                sh.send(Job::LoadSaved);
                sh.send(Job::Discover);
            }
        }
        let (lang, appearance) = {
            let sh = self.shared.lock().unwrap();
            (sh.language, sh.appearance)
        };
        ctx.set_theme(match appearance {
            Appearance::System => egui::ThemePreference::System,
            Appearance::Light => egui::ThemePreference::Light,
            Appearance::Dark => egui::ThemePreference::Dark,
        });

        keyboard_shortcuts(&ctx, &self.shared);
        handle_dropped_files(&ctx, &self.shared, lang);

        let wide = ui.available_width() >= 760.0;
        egui::Panel::top("header").show(ui, |ui| header(ui, &self.shared, lang, &mut self.settings_open));
        egui::Panel::bottom("status").show(ui, |ui| status_bar(ui, &self.shared, lang));
        if wide {
            egui::Panel::left("sidebar").resizable(true).default_size(260.0).size_range(220.0..=380.0).show(ui, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    ui.add_space(6.0);
                    sidebar(ui, &self.shared, lang);
                });
            });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                ui.add_space(6.0);
                if !wide {
                    let none_selected = self.shared.lock().unwrap().selected.is_empty();
                    egui::CollapsingHeader::new(lang.text("Speakers & inputs", "Speakers en ingangen"))
                        .default_open(none_selected)
                        .show(ui, |ui| sidebar(ui, &self.shared, lang));
                    ui.add_space(6.0);
                }
                if ui.available_width() >= 620.0 {
                    ui.columns(2, |cols| {
                        now_playing(&mut cols[0], &self.shared, lang, false);
                        group_section(&mut cols[0], &self.shared, lang);
                        details(&mut cols[1], &self.shared, lang);
                    });
                } else {
                    now_playing(ui, &self.shared, lang, false);
                    group_section(ui, &self.shared, lang);
                    details(ui, &self.shared, lang);
                }
            });
        });
        drop_overlay(&ctx, &self.shared, lang);

        settings_window(&ctx, &self.shared, &mut self.settings_open, lang);
        ip_dialog_window(&ctx, &self.shared, lang);
        browse_window(&ctx, &self.shared, lang);
        confirm_dialog(&ctx, &self.shared, lang);

        if self.shared.lock().unwrap().player_open {
            let shared = self.shared.clone();
            ctx.show_viewport_deferred(
                player_viewport_id(),
                egui::ViewportBuilder::default()
                    .with_title(lang.text("Mini player", "Minispeler"))
                    .with_inner_size([380.0, 520.0])
                    .with_min_inner_size([300.0, 320.0]),
                move |ui, _| player_ui(ui, &shared),
            );
        }

        // Repaint only when needed: the worker wakes us on events; the clock
        // and notice expiry need a tick of their own.
        {
            let sh = self.shared.lock().unwrap();
            if sh.dev().is_some_and(|d| d.playing && d.on_netusb()) {
                ctx.request_repaint_after(Duration::from_millis(500));
            }
            if let Some((_, at)) = &sh.notice {
                let left = Duration::from_secs(NOTICE_SECS).saturating_sub(at.elapsed());
                if !left.is_zero() {
                    ctx.request_repaint_after(left);
                }
            }
        }

        if self.screenshot.is_some() {
            ctx.request_repaint();
            self.frames += 1;
            if self.frames == 120 {
                let path = self.screenshot.clone().unwrap();
                let done = self.shot_done.clone();
                ctx.request_screenshot(move |img| {
                    let rgba = image::RgbaImage::from_fn(img.width() as u32, img.height() as u32, |x, y| {
                        image::Rgba(img.pixels[(y as usize) * img.width() + (x as usize)].to_array())
                    });
                    if let Err(e) = rgba.save(path) {
                        eprintln!("screenshot save failed: {e}");
                    }
                    done.store(true, std::sync::atomic::Ordering::SeqCst);
                });
            }
            if self.shot_done.load(std::sync::atomic::Ordering::SeqCst) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
    }
}

// ---------------------------------------------------------------- helpers

fn send(shared: &SharedArc, job: Job) {
    shared.lock().unwrap().send(job);
}

fn card(ui: &mut egui::Ui, title: &str, add: impl FnOnce(&mut egui::Ui)) {
    egui::Frame::group(ui.style()).inner_margin(12.0).corner_radius(10.0).show(ui, |ui| {
        ui.set_width(ui.available_width());
        if !title.is_empty() {
            ui.label(RichText::new(title).size(15.0).strong());
            ui.add_space(4.0);
        }
        add(ui);
    });
    ui.add_space(8.0);
}

fn status(dev: &Device, lang: Language) -> (&'static str, Color32) {
    if !dev.alive {
        (lang.text("Offline", "Offline"), Color32::GRAY)
    } else if dev.power_on {
        (lang.text("On", "Aan"), GREEN)
    } else {
        (lang.text("Standby", "Stand-by"), ORANGE)
    }
}

/// Text with a coloured status dot in front.
fn dot_label(ui: &egui::Ui, color: Color32, text: &str) -> egui::text::LayoutJob {
    let mut job = egui::text::LayoutJob::default();
    let font = egui::TextStyle::Body.resolve(ui.style());
    job.append("● ", 0.0, egui::TextFormat { font_id: font.clone(), color, ..Default::default() });
    job.append(text, 0.0, egui::TextFormat { font_id: font, color: ui.visuals().weak_text_color(), ..Default::default() });
    job
}

fn send_volume(shared: &SharedArc, volume: i64) {
    let mut sh = shared.lock().unwrap();
    if let Some(d) = sh.dev_mut() {
        let v = step_val(volume, d.volume_step.max(1)).clamp(d.volume_min, d.volume_max.max(d.volume_min + 1));
        d.volume = v;
        sh.send(Job::SetVolume(v));
    }
}

/// Space: play/pause, ↑/↓: volume, M: mute (not while typing).
fn keyboard_shortcuts(ctx: &egui::Context, shared: &SharedArc) {
    if ctx.text_edit_focused() {
        return;
    }
    let none = egui::Modifiers::NONE;
    let (space, up, down, m) = ctx.input_mut(|i| {
        (
            i.consume_key(none, egui::Key::Space),
            i.consume_key(none, egui::Key::ArrowUp),
            i.consume_key(none, egui::Key::ArrowDown),
            i.consume_key(none, egui::Key::M),
        )
    });
    let Some(d) = shared.lock().unwrap().dev().cloned() else { return };
    if !d.alive || !d.power_on {
        return;
    }
    if space && d.on_netusb() {
        send(shared, Job::Cmd(format!("/netusb/setPlayback?playback={}", if d.playing { "pause" } else { "play" })));
    }
    if up || down {
        let step = d.volume_step.max(1) * if d.volume_max > 100 { 2 } else { 1 };
        send_volume(shared, d.volume + if up { step } else { -step });
    }
    if m {
        send(shared, Job::Cmd(d.zpath(&format!("setMute?enable={}", !d.muted))));
    }
}

fn handle_dropped_files(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let paths: Vec<std::path::PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
    if paths.is_empty() {
        return;
    }
    let mut sh = shared.lock().unwrap();
    if sh.dev().is_some_and(|d| d.alive) {
        sh.send(Job::PlayPaths(expand_folders(paths)));
    } else {
        sh.note(lang.text("Select a speaker that is online first.", "Selecteer eerst een speaker die online is."));
    }
}

/// Dropped folders contribute their (sorted) files, one level deep.
fn expand_folders(paths: Vec<std::path::PathBuf>) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut files: Vec<_> = std::fs::read_dir(&p)
                .map(|rd| rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_file()).collect())
                .unwrap_or_default();
            files.sort();
            out.extend(files);
        } else {
            out.push(p);
        }
    }
    out
}

fn drop_overlay(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    if ctx.input(|i| i.raw.hovered_files.is_empty()) {
        return;
    }
    let name = shared.lock().unwrap().dev().map(|d| d.display_name()).unwrap_or_default();
    let screen = ctx.content_rect();
    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::unique("drop")));
    painter.rect_filled(screen, 0.0, Color32::from_black_alpha(170));
    painter.text(
        screen.center(),
        egui::Align2::CENTER_CENTER,
        format!("{} {name}", lang.text("Drop to play on", "Loslaten om af te spelen op")),
        egui::FontId::proportional(22.0),
        Color32::WHITE,
    );
}

// ---------------------------------------------------------------- header & status

fn header(ui: &mut egui::Ui, shared: &SharedArc, lang: Language, settings_open: &mut bool) {
    let dev = shared.lock().unwrap().dev().cloned();
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        match &dev {
            Some(d) => {
                ui.label(RichText::new(d.display_name()).size(20.0).strong());
                let (text, color) = status(d, lang);
                ui.label(dot_label(ui, color, text));
                if d.alive {
                    let (label, on) = if d.power_on {
                        (lang.text("⏻  Turn off", "⏻  Uitschakelen"), false)
                    } else {
                        (lang.text("⏻  Turn on", "⏻  Inschakelen"), true)
                    };
                    if ui.button(label).clicked() {
                        send(shared, Job::SetPower(on));
                    }
                }
            }
            None => {
                ui.label(RichText::new("MusicCast").size(20.0).strong());
            }
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("ℹ").on_hover_text(lang.text("About", "Over")).clicked() {
                show_about(lang);
            }
            if ui.button("⚙").on_hover_text(lang.text("Settings", "Instellingen")).clicked() {
                *settings_open = true;
            }
            let player_open = shared.lock().unwrap().player_open;
            if ui
                .add_enabled(dev.is_some() && !player_open, egui::Button::new("⧉"))
                .on_hover_text(lang.text("Open mini player", "Minispeler openen"))
                .clicked()
            {
                shared.lock().unwrap().player_open = true;
            }
        });
    });
    ui.add_space(4.0);
}

fn status_bar(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let (notice, searching) = {
        let sh = shared.lock().unwrap();
        (sh.notice.clone(), sh.searching)
    };
    ui.horizontal(|ui| {
        ui.set_min_height(24.0);
        let fresh = notice.filter(|(_, at)| at.elapsed() < Duration::from_secs(NOTICE_SECS));
        if let Some((msg, _)) = fresh {
            ui.label(msg);
        } else if searching {
            ui.spinner();
            ui.weak(lang.text("Searching for speakers…", "Speakers zoeken…"));
        } else {
            ui.weak(lang.text(
                "Space play/pause · ↑ ↓ volume · M mute · drop music files to play them",
                "Spatie afspelen/pauze · ↑ ↓ volume · M dempen · sleep muziekbestanden hierheen",
            ));
        }
    });
}

// ---------------------------------------------------------------- sidebar

fn sidebar(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let (devices, selected, searching, searched) = {
        let sh = shared.lock().unwrap();
        (sh.devices.clone(), sh.selected.clone(), sh.searching, sh.searched)
    };
    ui.label(RichText::new(lang.text("Speakers", "Speakers")).size(15.0).strong());
    if devices.is_empty() {
        if searching || !searched {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.weak(lang.text("Searching…", "Zoeken…"));
            });
        } else {
            ui.weak(lang.text(
                "No speakers found. Make sure your speaker is on and on the same network, or add it by IP address.",
                "Geen speakers gevonden. Controleer of je speaker aan staat en op hetzelfde netwerk zit, of voeg hem toe via het IP-adres.",
            ));
        }
    }
    for dev in &devices {
        let (st, color) = status(dev, lang);
        let mut job = egui::text::LayoutJob::default();
        let body = egui::TextStyle::Body.resolve(ui.style());
        let small = egui::FontId::proportional(body.size - 2.0);
        job.append(
            &dev.display_name(),
            0.0,
            egui::TextFormat { font_id: body.clone(), color: ui.visuals().strong_text_color(), ..Default::default() },
        );
        let mut detail = [dev.model.as_str(), dev.ip.as_str()].iter().filter(|s| !s.is_empty()).cloned().collect::<Vec<_>>().join(" · ");
        if dev.is_link_server() || dev.is_link_client() {
            detail = format!("🔗 {detail}");
        }
        job.append(
            &format!("\n{detail}"),
            0.0,
            egui::TextFormat { font_id: small.clone(), color: ui.visuals().weak_text_color(), ..Default::default() },
        );
        let tex = shared.lock().unwrap().textures.get(&dev.icon_url).cloned();
        let image = tex.map(|t| egui::Image::new((t.id(), Vec2::splat(30.0))).corner_radius(4.0));
        let dot = RichText::new("●").color(color);
        let button = egui::Button::opt_image_and_text(image, Some(job.into()))
            .selected(dev.ip == selected)
            .frame_when_inactive(false)
            .right_text(dot)
            .min_size(Vec2::new(ui.available_width(), 40.0));
        let hover = if dev.serial.is_empty() { st.to_string() } else { format!("{st}\n{}: {}", lang.text("Serial", "Serienummer"), dev.serial) };
        if ui.add(button).on_hover_text(hover).clicked() && dev.ip != selected {
            send(shared, Job::Select(dev.ip.clone()));
        }
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        if ui
            .add_enabled(!searching, egui::Button::new(lang.text("⟳ Search", "⟳ Zoeken")))
            .on_hover_text(lang.text("Search the local network again", "Opnieuw zoeken op het lokale netwerk"))
            .clicked()
        {
            send(shared, Job::Discover);
        }
        if ui.button(lang.text("+ Add by IP…", "+ Via IP…")).clicked() {
            shared.lock().unwrap().ip_dialog = Some(IpDialog { ip: String::new(), result: None, testing: false });
        }
        let sel = devices.iter().find(|d| d.ip == selected);
        if ui
            .add_enabled(sel.is_some(), egui::Button::new("🗑"))
            .on_hover_text(lang.text("Remove selected speaker", "Geselecteerde speaker verwijderen"))
            .clicked()
        {
            if let Some(d) = sel {
                shared.lock().unwrap().confirm = Some(Confirm::Remove { ip: d.ip.clone(), name: d.display_name() });
            }
        }
    });

    let dev = shared.lock().unwrap().dev().cloned();
    let Some(dev) = dev else { return };
    if !dev.alive || !dev.power_on {
        return;
    }
    ui.add_space(10.0);
    if dev.zones.len() > 1 {
        ui.horizontal(|ui| {
            ui.label(RichText::new(lang.text("Zone", "Zone")).size(15.0).strong());
            egui::ComboBox::from_id_salt("zone").selected_text(zone_title(&dev.zone)).show_ui(ui, |ui| {
                for zone in &dev.zones {
                    if ui.selectable_label(zone == &dev.zone, zone_title(zone)).clicked() && zone != &dev.zone {
                        send(shared, Job::SetZone(zone.clone()));
                    }
                }
            });
        });
        ui.add_space(6.0);
    }
    ui.label(RichText::new(lang.text("Input", "Ingang")).size(15.0).strong());
    if dev.inputs.is_empty() {
        ui.weak(lang.text("No inputs available.", "Geen ingangen beschikbaar."));
    }
    for input in &dev.inputs {
        let button = egui::Button::selectable(input.id == dev.input, input.title.as_str())
            .frame_when_inactive(false)
            .min_size(Vec2::new(ui.available_width(), 0.0))
            .right_text("");
        if ui.add(button).clicked() && input.id != dev.input {
            let mut sh = shared.lock().unwrap();
            if let Some(d) = sh.dev_mut() {
                d.input = input.id.clone();
            }
            sh.send(Job::Cmd(dev.zpath(&format!("setInput?input={}", input.id))));
        }
    }
}

fn zone_title(zone: &str) -> String {
    match zone {
        "main" => "Main".into(),
        z => input_display_name(z).replace("Zone", "Zone "),
    }
    .replace("  ", " ")
}

// ---------------------------------------------------------------- now playing

fn now_playing(ui: &mut egui::Ui, shared: &SharedArc, lang: Language, in_player: bool) {
    let dev = shared.lock().unwrap().dev().cloned();
    let Some(d) = dev else {
        card(ui, "", |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(30.0);
                ui.label(RichText::new("🔊").size(40.0));
                ui.label(RichText::new(lang.text("Select a speaker", "Kies een speaker")).size(18.0).strong());
                ui.weak(lang.text("Speakers on your network appear on the left.", "Speakers op je netwerk verschijnen links."));
                ui.add_space(30.0);
            });
        });
        return;
    };
    if !d.alive {
        card(ui, "", |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.label(RichText::new(lang.text("Speaker offline", "Speaker offline")).size(18.0).strong());
                ui.weak(lang.text(
                    "It may be unplugged or on another network. It reconnects automatically when it is back.",
                    "Misschien staat de stekker eruit of zit hij op een ander netwerk. Zodra hij terug is, maakt de app vanzelf weer verbinding.",
                ));
                ui.add_space(24.0);
            });
        });
        return;
    }
    if !d.power_on {
        card(ui, "", |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space(24.0);
                ui.weak(lang.text("This speaker is in standby.", "Deze speaker staat in stand-by."));
                ui.add_space(6.0);
                let b = egui::Button::new(RichText::new(lang.text("⏻  Turn on", "⏻  Inschakelen")).size(18.0))
                    .min_size(Vec2::new(180.0, 44.0));
                if ui.add(b).clicked() {
                    send(shared, Job::SetPower(true));
                }
                ui.add_space(24.0);
            });
        });
        return;
    }
    card(ui, if in_player { "" } else { lang.text("Now playing", "Nu speelt") }, |ui| {
        let wide = ui.available_width() >= 340.0;
        if wide {
            ui.horizontal_top(|ui| {
                artwork(ui, shared, &d, (ui.available_width() * 0.38).clamp(100.0, 200.0));
                ui.vertical(|ui| track_info(ui, shared, &d, lang));
            });
        } else {
            ui.vertical_centered(|ui| artwork(ui, shared, &d, ui.available_width().min(200.0)));
            track_info(ui, shared, &d, lang);
        }
        progress(ui, shared, &d);
        ui.add_space(6.0);
        transport(ui, shared, &d, lang);
        ui.add_space(4.0);
        volume_control(ui, shared, &d, lang);
        if d.on_netusb() && (!d.repeat_modes.is_empty() || !d.shuffle_modes.is_empty()) {
            modes(ui, shared, &d, lang);
        }
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if d.on_netusb() && !d.on_dlna() && ui.button(lang.text("📂 Browse", "📂 Bladeren")).clicked() {
                let mut sh = shared.lock().unwrap();
                sh.browse = Some(BrowseState { page: None, input: d.input.clone(), loading: true, error: String::new() });
                sh.send(Job::Browse { input: d.input.clone(), index: 0 });
            }
            if ui
                .button(lang.text("🎵 Play files…", "🎵 Bestanden afspelen…"))
                .on_hover_text(lang.text("Stream music files from this computer", "Muziekbestanden vanaf deze computer afspelen"))
                .clicked()
            {
                if let Some(paths) = rfd::FileDialog::new()
                    .add_filter(lang.text("Music", "Muziek"), &crate::stream::SUPPORTED_EXTS)
                    .pick_files()
                {
                    send(shared, Job::PlayPaths(paths));
                }
            }
        });
    });
}

fn artwork(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, side: f32) {
    let tex = shared.lock().unwrap().textures.get(&d.art_url).cloned();
    let size = Vec2::splat(side);
    match tex {
        Some(tex) => {
            ui.add(egui::Image::new((tex.id(), size)).corner_radius(8.0));
        }
        None => {
            let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
            let visuals = ui.visuals();
            ui.painter().rect_filled(rect, 8.0, visuals.faint_bg_color.gamma_multiply(2.0));
            ui.painter().rect_stroke(rect, 8.0, visuals.widgets.noninteractive.bg_stroke, egui::StrokeKind::Inside);
            let symbol = if d.on_tuner() { "📻" } else if d.on_netusb() { "♫" } else { "🔈" };
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                symbol,
                egui::FontId::proportional(side * 0.32),
                visuals.weak_text_color(),
            );
        }
    }
}

fn track_info(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    let _ = shared;
    let source = d.input_title(&d.input);
    let title_size = 19.0;
    if d.on_tuner() {
        let freq = tuner_freq_text(d);
        let title = if !d.tuner_station.is_empty() { d.tuner_station.clone() } else if !freq.is_empty() { freq.clone() } else { lang.text("Radio", "Radio").into() };
        ui.label(RichText::new(title).size(title_size).strong());
        if !d.tuner_station.is_empty() && !freq.is_empty() {
            ui.label(&freq);
        }
        if !d.tuner_text.is_empty() {
            ui.weak(&d.tuner_text);
        }
    } else if d.on_netusb() {
        if d.track.is_empty() {
            ui.label(RichText::new(lang.text("Nothing playing", "Er speelt niets")).size(title_size).strong());
        } else {
            ui.label(RichText::new(&d.track).size(title_size).strong());
        }
        if !d.artist.is_empty() {
            ui.label(&d.artist);
        }
        if !d.album.is_empty() {
            ui.weak(&d.album);
        }
    } else {
        ui.label(RichText::new(&source).size(title_size).strong());
        ui.weak(lang.text("External input", "Externe ingang"));
    }
    ui.add_space(4.0);
    let mut line = source;
    if !d.au_format.is_empty() {
        line = format!("{line} · {} {}", d.au_format, d.au_fs).trim().to_string();
    }
    ui.small(line);
}

fn tuner_freq_text(d: &Device) -> String {
    if d.tuner_freq <= 0 {
        return String::new();
    }
    match d.band.to_lowercase().as_str() {
        "fm" => format!("FM {:.2} MHz", d.tuner_freq as f64 / 1000.0).replace("0 MHz", " MHz").replace("  ", " "),
        "am" => format!("AM {} kHz", d.tuner_freq),
        _ => String::new(),
    }
}

fn progress(ui: &mut egui::Ui, shared: &SharedArc, d: &Device) {
    if !d.on_netusb() {
        return;
    }
    let t = shared.lock().unwrap().play_time(d);
    if d.total_time > 0 {
        ui.add_space(6.0);
        let frac = (t as f32 / d.total_time as f32).clamp(0.0, 1.0);
        ui.add(egui::ProgressBar::new(frac).desired_height(6.0).corner_radius(3.0));
        ui.horizontal(|ui| {
            ui.small(fmt_time(t));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.small(fmt_time(d.total_time));
            });
        });
    } else if t > 0 {
        ui.small(fmt_time(t));
    }
}

fn big_button(ui: &mut egui::Ui, text: &str, enabled: bool) -> egui::Response {
    ui.add_enabled(enabled, egui::Button::new(RichText::new(text).size(20.0)).min_size(Vec2::new(52.0, 38.0)))
}

fn transport(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    let local_queue = {
        let sh = shared.lock().unwrap();
        d.on_dlna() && sh.local_current.is_some() && !sh.local_files.is_empty()
    };
    // Center the row: measure once, then pad.
    let width = 3.0 * 52.0 + 2.0 * ui.spacing().item_spacing.x;
    ui.horizontal(|ui| {
        ui.add_space(((ui.available_width() - width) / 2.0).max(0.0));
        if d.on_tuner() {
            if big_button(ui, "⏮", true).on_hover_text(lang.text("Previous preset", "Vorige favoriet")).clicked() {
                send(shared, Job::Cmd("/tuner/switchPreset?dir=previous".into()));
            }
            let _ = big_button(ui, "📻", false);
            if big_button(ui, "⏭", true).on_hover_text(lang.text("Next preset", "Volgende favoriet")).clicked() {
                send(shared, Job::Cmd("/tuner/switchPreset?dir=next".into()));
            }
        } else if d.on_netusb() {
            if big_button(ui, "⏮", true).on_hover_text(lang.text("Previous", "Vorige")).clicked() {
                if local_queue {
                    send(shared, Job::LocalPrev);
                } else {
                    send(shared, Job::Cmd("/netusb/setPlayback?playback=previous".into()));
                }
            }
            let (icon, tip, action) = if d.playing {
                ("⏸", lang.text("Pause", "Pauzeren"), "pause")
            } else {
                ("▶", lang.text("Play", "Afspelen"), "play")
            };
            if big_button(ui, icon, true).on_hover_text(tip).clicked() {
                let mut sh = shared.lock().unwrap();
                if let Some(dev) = sh.dev_mut() {
                    dev.playing = action == "play";
                }
                sh.send(Job::Cmd(format!("/netusb/setPlayback?playback={action}")));
            }
            if big_button(ui, "⏭", true).on_hover_text(lang.text("Next", "Volgende")).clicked() {
                if local_queue {
                    send(shared, Job::LocalNext);
                } else {
                    send(shared, Job::Cmd("/netusb/setPlayback?playback=next".into()));
                }
            }
        }
    });
}

fn volume_control(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    ui.horizontal(|ui| {
        let (icon, tip) = if d.muted {
            ("🔇", lang.text("Unmute (M)", "Geluid aan (M)"))
        } else {
            ("🔊", lang.text("Mute (M)", "Dempen (M)"))
        };
        if ui.add(egui::Button::new(RichText::new(icon).size(16.0)).selected(d.muted)).on_hover_text(tip).clicked() {
            send(shared, Job::Cmd(d.zpath(&format!("setMute?enable={}", !d.muted))));
        }
        let min = d.volume_min;
        let max = d.volume_max.max(min + 1);
        let mut volume = shared.lock().unwrap().ui_volume.unwrap_or(d.volume);
        let value_text = match &d.volume_db {
            Some((db, unit)) if shared.lock().unwrap().ui_volume.is_none() => format!("{db:.1} {unit}"),
            _ => volume.to_string(),
        };
        ui.spacing_mut().slider_width = (ui.available_width() - 80.0).max(60.0);
        let response = ui.add(egui::Slider::new(&mut volume, min..=max).show_value(false)).on_hover_text(lang.text(
            "Volume (↑ ↓ or scroll)",
            "Volume (↑ ↓ of scrollen)",
        ));
        ui.label(RichText::new(value_text).monospace());
        if response.dragged() {
            shared.lock().unwrap().ui_volume = Some(volume);
        }
        if response.drag_stopped() || (response.changed() && !response.dragged()) {
            shared.lock().unwrap().ui_volume = None;
            send_volume(shared, volume);
        }
        // Scroll wheel over the slider changes the volume instead of scrolling the page.
        if response.hovered() {
            let dy = ui.input(|i| i.smooth_scroll_delta.y);
            if dy != 0.0 {
                ui.input_mut(|i| i.smooth_scroll_delta = Vec2::ZERO);
                let mut sh = shared.lock().unwrap();
                sh.scroll_acc += dy;
                let steps = (sh.scroll_acc / 24.0).trunc() as i64;
                if steps != 0 {
                    sh.scroll_acc -= steps as f32 * 24.0;
                    drop(sh);
                    send_volume(shared, d.volume + steps * d.volume_step.max(1));
                }
            }
        }
    });
}

fn modes(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    ui.horizontal_wrapped(|ui| {
        if !d.repeat_modes.is_empty() {
            let (icon, text) = match d.repeat.as_str() {
                "one" => ("🔂", lang.text("Repeat one", "Eén herhalen")),
                "all" => ("🔁", lang.text("Repeat all", "Alles herhalen")),
                _ => ("🔁", lang.text("Repeat off", "Herhalen uit")),
            };
            let b = egui::Button::selectable(d.repeat != "off" && !d.repeat.is_empty(), format!("{icon} {text}"));
            if ui.add(b).on_hover_text(lang.text("Click to change", "Klik om te wijzigen")).clicked() {
                let modes = &d.repeat_modes;
                let next = modes
                    .iter()
                    .position(|m| *m == d.repeat)
                    .map(|i| modes[(i + 1) % modes.len()].clone())
                    .unwrap_or_else(|| modes[0].clone());
                send(shared, Job::SetRepeat(next));
            }
        }
        if !d.shuffle_modes.is_empty() {
            let on = d.shuffle != "off" && !d.shuffle.is_empty();
            let b = egui::Button::selectable(on, format!("🔀 {}", lang.text("Shuffle", "Willekeurig")));
            if ui.add(b).clicked() {
                send(shared, Job::SetShuffle(!on));
            }
        }
    });
}

// ---------------------------------------------------------------- details column

fn details(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let dev = shared.lock().unwrap().dev().cloned();
    let Some(d) = dev else { return };
    if !d.alive || !d.power_on {
        return;
    }
    if d.has_tuner() && d.on_tuner() {
        radio_card(ui, shared, &d, lang);
    } else if !d.presets.is_empty() {
        card(ui, lang.text("Presets", "Favorieten"), |ui| {
            preset_grid(ui, shared, &d, &d.presets, false, lang);
        });
    }
    sound_card(ui, shared, &d, lang);
    local_files_card(ui, shared, lang);
}

/// One-click presets; right-click a slot to store what is playing.
fn preset_grid(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, presets: &[Preset], tuner: bool, lang: Language) {
    let spacing = ui.spacing().item_spacing.x;
    let cols = if ui.available_width() > 360.0 { 2 } else { 1 };
    let col_w = (ui.available_width() - spacing * (cols as f32 - 1.0)) / cols as f32;
    egui::Grid::new(if tuner { "tuner_presets" } else { "presets" }).spacing([spacing, 6.0]).show(ui, |ui| {
        for (i, p) in presets.iter().enumerate() {
            let empty = p.is_empty();
            let band = if tuner && d.tuner_presets_common() { format!("{} ", p.band.to_uppercase()) } else { String::new() };
            let text = if empty {
                RichText::new(format!("{:>2}  {}", p.num, lang.text("Empty", "Leeg"))).weak()
            } else {
                RichText::new(format!("{:>2}  {band}{}", p.num, p.text.trim()))
            };
            let resp = ui.add_sized([col_w, 30.0], egui::Button::new(text).truncate());
            let resp = if empty { resp } else { resp.on_hover_text(p.text.trim()) };
            if resp.clicked() && !empty {
                let path = if tuner {
                    format!("/tuner/recallPreset?zone={}&band={}&num={}", d.zone_or_main(), p.band, p.num)
                } else {
                    format!("/netusb/recallPreset?zone={}&num={}", d.zone_or_main(), p.num)
                };
                send(shared, Job::Cmd(path));
            }
            resp.context_menu(|ui| {
                // Separate tuner lists store into the slot of the band that is playing.
                let allowed = !tuner || d.tuner_presets_common() || p.band.eq_ignore_ascii_case(&d.band);
                if ui
                    .add_enabled(allowed, egui::Button::new(lang.text("Save what's playing here…", "Huidige hier opslaan…")))
                    .clicked()
                {
                    shared.lock().unwrap().confirm =
                        Some(Confirm::SavePreset { ip: d.ip.clone(), tuner, band: p.band.clone(), num: p.num });
                    ui.close();
                }
                if !allowed {
                    ui.weak(lang.text("Switch to this band first.", "Kies eerst deze band."));
                }
            });
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
    ui.add_space(2.0);
    ui.weak(lang.text("Right-click a preset to save what's playing.", "Klik met rechts op een favoriet om op te slaan wat er speelt."));
}

fn radio_card(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    card(ui, lang.text("Radio", "Radio"), |ui| {
        if d.bands.len() > 1 {
            ui.horizontal(|ui| {
                for band in &d.bands {
                    if ui.add(egui::Button::selectable(band.eq_ignore_ascii_case(&d.band), band.to_uppercase())).clicked()
                        && !band.eq_ignore_ascii_case(&d.band)
                    {
                        send(shared, Job::Cmd(format!("/tuner/setBand?band={band}")));
                    }
                }
            });
        }
        if let Some(range) = d.current_band_range() {
            ui.horizontal(|ui| {
                if ui.button("◀").on_hover_text(lang.text("Tune down", "Omlaag afstemmen")).clicked() {
                    send(shared, Job::Cmd(format!("/tuner/setFreq?band={}&tuning=down", d.band)));
                }
                let mut freq = shared.lock().unwrap().ui_freq.unwrap_or(d.tuner_freq).clamp(range.min, range.max);
                let shown = Device { tuner_freq: freq, ..d.clone() };
                ui.spacing_mut().slider_width = (ui.available_width() - 150.0).max(60.0);
                let response = ui.add(egui::Slider::new(&mut freq, range.min..=range.max).show_value(false));
                if ui.button("▶").on_hover_text(lang.text("Tune up", "Omhoog afstemmen")).clicked() {
                    send(shared, Job::Cmd(format!("/tuner/setFreq?band={}&tuning=up", d.band)));
                }
                ui.label(RichText::new(tuner_freq_text(&shown)).monospace());
                if response.dragged() {
                    shared.lock().unwrap().ui_freq = Some(freq);
                }
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    let freq = step_val(freq - range.min, range.step.max(1)) + range.min;
                    let freq = freq.clamp(range.min, range.max);
                    let mut sh = shared.lock().unwrap();
                    sh.ui_freq = None;
                    if let Some(dev) = sh.dev_mut() {
                        dev.tuner_freq = freq;
                    }
                    sh.send(Job::Cmd(format!("/tuner/setFreq?band={}&tuning=direct&num={freq}", d.band)));
                }
            });
        }
        ui.add_space(4.0);
        if d.tuner_presets.is_empty() {
            ui.weak(lang.text("No radio presets available.", "Geen radiofavorieten beschikbaar."));
        } else {
            let list: Vec<Preset> = if d.tuner_presets_common() {
                d.tuner_presets.clone()
            } else {
                d.tuner_presets.iter().filter(|p| p.band.eq_ignore_ascii_case(&d.band)).cloned().collect()
            };
            preset_grid(ui, shared, d, &list, true, lang);
        }
    });
}

fn group_section(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let dev = shared.lock().unwrap().dev().cloned();
    if let Some(d) = dev.filter(|d| d.alive) {
        link_card(ui, shared, &d, lang);
    }
}

/// MusicCast Link: let other speakers play along with the selected one.
fn link_card(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    let devices = shared.lock().unwrap().devices.clone();
    let others: Vec<&Device> = devices.iter().filter(|o| o.ip != d.ip).collect();
    if others.is_empty() {
        return;
    }
    let name_of = |ip: &str| devices.iter().find(|x| x.ip == ip).map(|x| x.display_name()).unwrap_or_else(|| ip.to_string());
    let server_of = |c: &Device| devices.iter().find(|s| s.is_link_server() && s.link_group_id == c.link_group_id);
    card(ui, lang.text("Multiroom", "Multiroom"), |ui| {
        if d.is_link_client() {
            let server = server_of(d);
            let name = server.map(|s| s.display_name()).unwrap_or_else(|| lang.text("another speaker", "een andere speaker").into());
            ui.label(format!("🔗 {} {name}", lang.text("Plays along with", "Speelt mee met")));
            ui.horizontal(|ui| {
                if let Some(s) = server {
                    if ui.button(lang.text("Go to group", "Naar de groep")).clicked() {
                        send(shared, Job::Select(s.ip.clone()));
                    }
                }
                if ui.button(lang.text("Leave group", "Groep verlaten")).clicked() {
                    send(shared, Job::LinkRemove(d.ip.clone()));
                }
            });
            return;
        }
        ui.weak(lang.text(
            "Let other speakers play along with this one:",
            "Laat andere speakers meespelen met deze speaker:",
        ));
        ui.add_space(2.0);
        for o in &others {
            let member = d.link_clients.contains(&o.ip);
            let note = if !o.alive {
                lang.text("offline", "offline").to_string()
            } else if !member && o.is_link_client() {
                let leader = server_of(o).map(|s| s.display_name()).unwrap_or_default();
                format!("{} {leader}", lang.text("plays along with", "speelt mee met")).trim().to_string()
            } else if o.is_link_server() {
                lang.text("leads its own group", "heeft een eigen groep").to_string()
            } else {
                String::new()
            };
            ui.horizontal(|ui| {
                let mut checked = member;
                let r = ui.add_enabled(o.alive || member, egui::Checkbox::new(&mut checked, o.display_name()));
                if !note.is_empty() {
                    ui.weak(note);
                }
                if r.changed() {
                    send(shared, if checked { Job::LinkAdd(vec![o.ip.clone()]) } else { Job::LinkRemove(o.ip.clone()) });
                }
            });
            if member && o.alive {
                ui.horizontal(|ui| {
                    ui.add_space(26.0);
                    member_volume(ui, shared, o, lang);
                });
            }
        }
        // Clients the device reports but this app doesn't know (e.g. another subnet).
        for ip in d.link_clients.iter().filter(|ip| !devices.iter().any(|x| &x.ip == *ip)) {
            ui.horizontal(|ui| {
                ui.label(format!("🔗 {}", name_of(ip)));
                if ui.small_button(lang.text("Remove", "Verwijderen")).clicked() {
                    send(shared, Job::LinkRemove(ip.clone()));
                }
            });
        }
        let free: Vec<String> = others.iter().filter(|o| o.alive && !d.link_clients.contains(&o.ip)).map(|o| o.ip.clone()).collect();
        ui.add_space(4.0);
        ui.horizontal_wrapped(|ui| {
            if free.len() > 1 && ui.button(lang.text("Add all", "Alles toevoegen")).clicked() {
                send(shared, Job::LinkAdd(free.clone()));
            }
            if d.is_link_server() {
                ui.label(lang.text("Group volume", "Groepsvolume"));
                for (label, dir) in [("−", -1i64), ("+", 1)] {
                    if ui.button(label).on_hover_text(lang.text("All speakers in the group", "Alle speakers in de groep")).clicked() {
                        group_volume_step(shared, d, dir);
                    }
                }
                if ui.button(lang.text("End group", "Groep opheffen")).clicked() {
                    send(shared, Job::LinkDissolve);
                }
            }
        });
    });
}

fn member_volume(ui: &mut egui::Ui, shared: &SharedArc, o: &Device, lang: Language) {
    let min = o.volume_min;
    let max = o.volume_max.max(min + 1);
    let mut volume = shared.lock().unwrap().ui_member_volume.get(&o.ip).copied().unwrap_or(o.volume);
    ui.spacing_mut().slider_width = (ui.available_width() - 50.0).max(60.0);
    let response = ui
        .add(egui::Slider::new(&mut volume, min..=max).show_value(false))
        .on_hover_text(format!("{} {}", lang.text("Volume of", "Volume van"), o.display_name()));
    ui.label(RichText::new(volume.to_string()).monospace());
    if response.dragged() {
        shared.lock().unwrap().ui_member_volume.insert(o.ip.clone(), volume);
    }
    if response.drag_stopped() || (response.changed() && !response.dragged()) {
        set_member_volume(shared, &o.ip, volume);
    }
}

fn set_member_volume(shared: &SharedArc, ip: &str, volume: i64) {
    let mut sh = shared.lock().unwrap();
    sh.ui_member_volume.remove(ip);
    let Some(dev) = sh.devices.iter_mut().find(|x| x.ip == ip) else { return };
    let v = step_val(volume, dev.volume_step.max(1)).clamp(dev.volume_min, dev.volume_max.max(dev.volume_min + 1));
    dev.volume = v;
    let path = dev.zpath(&format!("setVolume?volume={v}"));
    sh.send(Job::CmdFor { ip: ip.to_string(), path });
}

/// Nudge every speaker in the group by about 2% of its own range.
fn group_volume_step(shared: &SharedArc, server: &Device, dir: i64) {
    let step = |d: &Device| (((d.volume_max - d.volume_min) as f64 * 0.02).round() as i64).max(d.volume_step.max(1));
    send_volume(shared, server.volume + dir * step(server));
    let members: Vec<Device> = {
        let sh = shared.lock().unwrap();
        sh.devices.iter().filter(|x| server.link_clients.contains(&x.ip) && x.alive).cloned().collect()
    };
    for m in members {
        set_member_volume(shared, &m.ip, m.volume + dir * step(&m));
    }
}

fn sound_card(ui: &mut egui::Ui, shared: &SharedArc, d: &Device, lang: Language) {
    let toggles: Vec<(&str, &str, &str, bool)> = [
        ("Enhancer", "enhancer", "Enhancer", d.enhancer),
        ("Direct", "direct", "Direct", d.direct),
        ("BassExtension", "bass_extension", lang.text("Bass extension", "Extra bas"), d.bass_extension),
        ("ClearVoice", "clear_voice", "Clear Voice", d.clear_voice),
    ]
    .into_iter()
    .filter(|(_, f, _, _)| d.func_list.iter().any(|x| x == f))
    .collect();
    let has_dimmer = d.sys_func_list.iter().any(|f| f == "dimmer");
    if d.programs.is_empty() && toggles.is_empty() && d.tones.is_empty() && !has_dimmer {
        return;
    }
    card(ui, lang.text("Sound", "Geluid"), |ui| {
        if !d.programs.is_empty() {
            ui.horizontal(|ui| {
                ui.label(lang.text("Sound program", "Geluidsprogramma"));
                egui::ComboBox::from_id_salt("program")
                    .selected_text(input_display_name(&d.sound_program))
                    .width(ui.available_width().max(80.0))
                    .show_ui(ui, |ui| {
                        for program in &d.programs {
                            if ui.selectable_label(program == &d.sound_program, input_display_name(program)).clicked() {
                                send(shared, Job::Cmd(d.zpath(&format!("setSoundProgram?program={program}"))));
                            }
                        }
                    });
            });
        }
        if !toggles.is_empty() {
            ui.horizontal_wrapped(|ui| {
                for (name, _, title, on) in &toggles {
                    if ui.add(egui::Button::selectable(*on, *title)).clicked() {
                        let mut sh = shared.lock().unwrap();
                        if let Some(dev) = sh.dev_mut() {
                            match *name {
                                "Enhancer" => dev.enhancer = !on,
                                "Direct" => dev.direct = !on,
                                "BassExtension" => dev.bass_extension = !on,
                                _ => dev.clear_voice = !on,
                            }
                        }
                        sh.send(Job::Cmd(d.zpath(&format!("set{name}?enable={}", !on))));
                    }
                }
            });
        }
        if !d.tones.is_empty() {
            ui.add_space(4.0);
            egui::Grid::new("tones").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                for tone in &d.tones {
                    let min = d.tone_min;
                    let max = d.tone_max.max(min + 1);
                    let mut value = shared.lock().unwrap().ui_tones.get(&tone.name).copied().unwrap_or(tone.value);
                    let name = match tone.name.as_str() {
                        "bass" => lang.text("Bass", "Bas"),
                        "treble" => lang.text("Treble", "Hoog"),
                        "low" => lang.text("Low", "Laag"),
                        "mid" => lang.text("Mid", "Midden"),
                        "high" => lang.text("High", "Hoog"),
                        other => other,
                    };
                    ui.label(name);
                    ui.spacing_mut().slider_width = (ui.available_width() - 50.0).max(60.0);
                    let response = ui
                        .add(egui::Slider::new(&mut value, min..=max))
                        .on_hover_text(lang.text("Double-click to reset", "Dubbelklik om te resetten"));
                    ui.end_row();
                    let reset = response.double_clicked();
                    if reset {
                        value = 0.clamp(min, max);
                    }
                    if response.dragged() && !reset {
                        shared.lock().unwrap().ui_tones.insert(tone.name.clone(), value);
                    }
                    if reset || response.drag_stopped() || (response.changed() && !response.dragged()) {
                        let value = step_val(value, d.tone_step.max(1)).clamp(min, max);
                        let mut sh = shared.lock().unwrap();
                        sh.ui_tones.remove(&tone.name);
                        if let Some(dev) = sh.dev_mut() {
                            if let Some(t) = dev.tones.iter_mut().find(|t| t.name == tone.name) {
                                t.value = value;
                            }
                        }
                        let endpoint = match d.tone_kind {
                            ToneKind::ToneControl => "setToneControl",
                            ToneKind::Equalizer => "setEqualizer",
                        };
                        sh.send(Job::Cmd(d.zpath(&format!("{endpoint}?mode=manual&{}={value}", tone.name))));
                    }
                }
            });
        }
        if has_dimmer {
            ui.add_space(4.0);
            let level = if d.dimmer < 0 { lang.text("Auto", "Automatisch").to_string() } else { d.dimmer.to_string() };
            if ui
                .button(format!("💡 {}: {level}", lang.text("Display brightness", "Displayhelderheid")))
                .on_hover_text(lang.text("Click to change", "Klik om te wijzigen"))
                .clicked()
            {
                let step = d.dim_step.max(1);
                let next = if d.dimmer + step > d.dim_max { d.dim_min } else { d.dimmer + step };
                send(shared, Job::Cmd(format!("/system/setDimmer?value={next}")));
            }
        }
    });
}

fn local_files_card(ui: &mut egui::Ui, shared: &SharedArc, lang: Language) {
    let (files, current, can_play) = {
        let sh = shared.lock().unwrap();
        (sh.local_files.clone(), sh.local_current, sh.dev().is_some_and(|d| d.alive))
    };
    if files.is_empty() {
        return;
    }
    card(ui, lang.text("Local files", "Lokale bestanden"), |ui| {
        egui::ScrollArea::vertical().id_salt("local").max_height(220.0).show(ui, |ui| {
            for (i, file) in files.iter().enumerate() {
                let playing = current == Some(i);
                let text = if playing { format!("▶ {file}") } else { format!("   {file}") };
                let b = egui::Button::selectable(playing, text)
                    .frame_when_inactive(false)
                    .truncate()
                    .min_size(Vec2::new(ui.available_width(), 0.0))
                    .right_text("");
                if ui.add_enabled(can_play, b).clicked() {
                    send(shared, Job::LocalJump(i));
                }
            }
        });
    });
}

// ---------------------------------------------------------------- windows

fn settings_window(ctx: &egui::Context, shared: &SharedArc, open: &mut bool, lang: Language) {
    if !*open {
        return;
    }
    egui::Window::new(lang.text("Settings", "Instellingen"))
        .id(egui::Id::unique("settings"))
        .open(open)
        .collapsible(false)
        .resizable(false)
        .default_width(340.0)
        .show(ctx, |ui| {
            let (mut language, mut appearance, mut seconds) = {
                let sh = shared.lock().unwrap();
                (sh.language, sh.appearance, sh.poll_secs)
            };
            egui::Grid::new("settings_grid").num_columns(2).spacing([12.0, 10.0]).show(ui, |ui| {
                ui.label(lang.text("Appearance", "Weergave"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut appearance, Appearance::System, lang.text("System", "Systeem"));
                    ui.selectable_value(&mut appearance, Appearance::Light, lang.text("Light", "Licht"));
                    ui.selectable_value(&mut appearance, Appearance::Dark, lang.text("Dark", "Donker"));
                });
                ui.end_row();
                ui.label(lang.text("Language", "Taal"));
                ui.horizontal(|ui| {
                    ui.selectable_value(&mut language, Language::English, "English");
                    ui.selectable_value(&mut language, Language::Dutch, "Nederlands");
                });
                ui.end_row();
                ui.label(lang.text("Refresh every", "Verversen elke"));
                let response = ui.add(egui::Slider::new(&mut seconds, 1..=20).suffix(" s"));
                ui.end_row();
                let mut sh = shared.lock().unwrap();
                if response.changed() {
                    sh.poll_secs = seconds;
                }
                // Save once the slider is released, not on every tick.
                if response.drag_stopped() || (response.changed() && !response.dragged()) {
                    sh.send(Job::SetPollSecs(seconds));
                }
            });
            ui.add_space(4.0);
            ui.weak(lang.text(
                "Speakers also report changes instantly; this interval is a fallback.",
                "Speakers melden wijzigingen ook direct; dit interval is een terugvaloptie.",
            ));
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

fn valid_host(s: &str) -> bool {
    let s = s.trim();
    !s.is_empty()
        && s.len() <= 253
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-')
        && !s.starts_with(['.', '-'])
}

fn ip_dialog_window(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let Some(mut dialog) = shared.lock().unwrap().ip_dialog.take() else { return };
    let mut open = true;
    let mut close = false;
    egui::Window::new(lang.text("Add speaker", "Speaker toevoegen"))
        .id(egui::Id::unique("add_ip"))
        .open(&mut open)
        .collapsible(false)
        .resizable(false)
        .default_width(340.0)
        .show(ctx, |ui| {
            ui.label(lang.text(
                "IP address or host name of your MusicCast speaker:",
                "IP-adres of hostnaam van je MusicCast-speaker:",
            ));
            let edit = ui.add(egui::TextEdit::singleline(&mut dialog.ip).hint_text("192.168.1.123").desired_width(f32::INFINITY));
            let valid = valid_host(&dialog.ip);
            let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if dialog.testing {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(lang.text("Testing…", "Bezig met testen…"));
                });
            } else {
                match &dialog.result {
                    Some(Ok(name)) => {
                        ui.colored_label(GREEN, format!("✔ {}: {name}", lang.text("Found", "Gevonden")));
                    }
                    Some(Err(e)) => {
                        ui.colored_label(ui.visuals().warn_fg_color, format!("✖ {}: {e}", lang.text("Test failed", "Test mislukt")));
                    }
                    None => {}
                }
            }
            if !dialog.ip.trim().is_empty() && !valid {
                ui.colored_label(ui.visuals().warn_fg_color, lang.text("Enter a valid address.", "Voer een geldig adres in."));
            }
            ui.horizontal(|ui| {
                if ui.add_enabled(valid && !dialog.testing, egui::Button::new(lang.text("Test", "Testen"))).clicked() {
                    dialog.testing = true;
                    dialog.result = None;
                    send(shared, Job::TestIp(dialog.ip.trim().to_string()));
                }
                if ui.add_enabled(valid && !dialog.testing, egui::Button::new(lang.text("Add", "Toevoegen"))).clicked()
                    || (enter && valid && !dialog.testing)
                {
                    send(shared, Job::AddManual(dialog.ip.trim().to_string()));
                    close = true;
                }
                if ui.button(lang.text("Cancel", "Annuleren")).clicked() {
                    close = true;
                }
            });
        });
    if open && !close {
        shared.lock().unwrap().ip_dialog = Some(dialog);
    }
}

fn browse_window(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let data = {
        let sh = shared.lock().unwrap();
        sh.browse.as_ref().map(|b| {
            (
                b.page.as_ref().map(|p| (p.entries.clone(), p.index, p.max_line, p.layer, p.name.clone())),
                b.input.clone(),
                b.loading,
                b.error.clone(),
            )
        })
    };
    let Some((page, input, loading, error)) = data else { return };
    let title = shared.lock().unwrap().dev().map(|d| d.input_title(&input)).unwrap_or_else(|| input_display_name(&input));
    let start_loading = |shared: &SharedArc, job: Job| {
        let mut sh = shared.lock().unwrap();
        if let Some(b) = &mut sh.browse {
            b.loading = true;
            b.error.clear();
        }
        sh.send(job);
    };
    let mut open = true;
    egui::Window::new(format!("{} — {title}", lang.text("Browse", "Bladeren")))
        .id(egui::Id::unique("browse"))
        .open(&mut open)
        .default_width(420.0)
        .show(ctx, |ui| {
            let (entries, base, max_line, layer, name) = page.unwrap_or_default_page();
            ui.horizontal(|ui| {
                if ui.add_enabled(layer > 0 && !loading, egui::Button::new(lang.text("⬅ Back", "⬅ Terug"))).clicked() {
                    start_loading(shared, Job::BrowseReturn);
                }
                ui.label(RichText::new(if name.is_empty() { title.clone() } else { name }).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if loading {
                        ui.spinner();
                    }
                });
            });
            ui.separator();
            if !error.is_empty() {
                ui.colored_label(ui.visuals().warn_fg_color, &error);
            } else if entries.is_empty() && !loading {
                ui.weak(lang.text("This list is empty.", "Deze lijst is leeg."));
            }
            for (i, entry) in entries.iter().enumerate() {
                let index = base + i as i64;
                ui.horizontal(|ui| {
                    let play = ui
                        .add_enabled(!loading, egui::Button::new("▶").small())
                        .on_hover_text(lang.text("Play", "Afspelen"));
                    let row = egui::Button::new(entry.text.as_str())
                        .frame_when_inactive(false)
                        .truncate()
                        .min_size(Vec2::new(ui.available_width(), 0.0))
                        .right_text("›");
                    if ui.add_enabled(!loading, row).on_hover_text(lang.text("Open", "Openen")).clicked() {
                        start_loading(shared, Job::BrowseSelect(index));
                    }
                    if play.clicked() {
                        send(shared, Job::BrowsePlay(index));
                    }
                });
            }
            if max_line > 8 {
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.add_enabled(base > 0 && !loading, egui::Button::new("‹")).clicked() {
                        start_loading(shared, Job::Browse { input: input.clone(), index: (base - 8).max(0) });
                    }
                    let last = (base + entries.len() as i64).min(max_line);
                    ui.label(format!("{}–{last} {} {max_line}", base + 1, lang.text("of", "van")));
                    if ui.add_enabled(last < max_line && !loading, egui::Button::new("›")).clicked() {
                        start_loading(shared, Job::Browse { input: input.clone(), index: base + 8 });
                    }
                });
            }
        });
    if !open {
        shared.lock().unwrap().browse = None;
    }
}

trait PageExt {
    fn unwrap_or_default_page(self) -> (Vec<ListEntry>, i64, i64, i64, String);
}
impl PageExt for Option<(Vec<ListEntry>, i64, i64, i64, String)> {
    fn unwrap_or_default_page(self) -> (Vec<ListEntry>, i64, i64, i64, String) {
        self.unwrap_or_default()
    }
}

fn confirm_dialog(ctx: &egui::Context, shared: &SharedArc, lang: Language) {
    let Some(choice) = shared.lock().unwrap().confirm.clone() else { return };
    let mut close = false;
    let (title, text) = match &choice {
        Confirm::SavePreset { num, tuner, band, .. } => (
            lang.text("Save preset?", "Favoriet opslaan?").to_string(),
            format!(
                "{} {num}{}?",
                lang.text("Replace preset", "Favoriet vervangen:"),
                if *tuner { format!(" ({})", band.to_uppercase()) } else { String::new() }
            ),
        ),
        Confirm::Remove { name, .. } => (
            lang.text("Remove speaker?", "Speaker verwijderen?").to_string(),
            format!(
                "{name} {}",
                lang.text(
                    "will be removed from the list. A search finds it again while it is on the network.",
                    "wordt uit de lijst gehaald. Zoeken vindt hem terug zolang hij op het netwerk zit."
                )
            ),
        ),
    };
    egui::Modal::new(egui::Id::unique("confirm")).show(ctx, |ui| {
        ui.set_max_width(320.0);
        ui.label(RichText::new(title).size(16.0).strong());
        ui.add_space(4.0);
        ui.label(text);
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let (label, valid) = match &choice {
                Confirm::SavePreset { ip, tuner, band, .. } => {
                    let sh = shared.lock().unwrap();
                    let ok = sh.dev().is_some_and(|d| {
                        d.ip == *ip && d.alive && (!tuner || (d.on_tuner() && (d.tuner_presets_common() || d.band.eq_ignore_ascii_case(band))))
                    });
                    (lang.text("Save", "Opslaan"), ok)
                }
                Confirm::Remove { .. } => (lang.text("Remove", "Verwijderen"), true),
            };
            if ui.add_enabled(valid, egui::Button::new(label)).clicked() {
                match &choice {
                    Confirm::SavePreset { tuner: false, num, .. } => send(shared, Job::Cmd(format!("/netusb/storePreset?num={num}"))),
                    Confirm::SavePreset { tuner: true, num, .. } => send(shared, Job::Cmd(format!("/tuner/storePreset?num={num}"))),
                    Confirm::Remove { ip, .. } => send(shared, Job::Remove(ip.clone())),
                }
                close = true;
            }
            if ui.button(lang.text("Cancel", "Annuleren")).clicked() || ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                close = true;
            }
        });
    });
    if close {
        shared.lock().unwrap().confirm = None;
    }
}

fn player_ui(ui: &mut egui::Ui, shared: &SharedArc) {
    if ui.input(|i| i.viewport().close_requested()) {
        shared.lock().unwrap().player_open = false;
    }
    let lang = shared.lock().unwrap().language;
    keyboard_shortcuts(ui.ctx(), shared);
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| now_playing(ui, shared, lang, true));
    });
}

// ---------------------------------------------------------------- demo data

/// Made-up speakers for `--demo` (screenshots without a network).
pub fn seed_demo(sh: &mut Shared) {
    let inputs: Vec<InputItem> = ["spotify", "net_radio", "airplay", "server", "bluetooth", "tuner", "optical"]
        .iter()
        .map(|id| InputItem {
            id: id.to_string(),
            title: input_display_name(id),
            play_info_type: match *id {
                "tuner" => "tuner",
                "optical" => "none",
                _ => "netusb",
            }
            .into(),
        })
        .collect();
    let presets = ["NPO Radio 2", "KINK", "Radio 538", "", "BBC Radio 6 Music", ""]
        .iter()
        .enumerate()
        .map(|(i, t)| Preset { num: i as i64 + 1, band: String::new(), text: t.to_string() })
        .collect();
    let living = Device {
        ip: "192.168.1.20".into(),
        name: "Woonkamer".into(),
        model: "R-N303D".into(),
        alive: true,
        initialised: true,
        power_on: true,
        volume: 34,
        volume_max: 161,
        volume_step: 1,
        volume_db: Some((-46.5, "dB".into())),
        input: "spotify".into(),
        playing: true,
        repeat: "all".into(),
        shuffle: "off".into(),
        repeat_modes: vec!["off".into(), "one".into(), "all".into()],
        shuffle_modes: vec!["off".into(), "on".into()],
        track: "Ghost in You, Pt. I".into(),
        artist: "protoU".into(),
        album: "Anomalies".into(),
        play_time: 83,
        total_time: 241,
        inputs,
        zones: vec!["main".into()],
        zone: "main".into(),
        func_list: vec!["enhancer".into(), "direct".into(), "bass_extension".into()],
        sys_func_list: vec!["dimmer".into()],
        programs: vec![],
        dimmer: 2,
        dim_min: -1,
        dim_max: 3,
        dim_step: 1,
        tone_min: -12,
        tone_max: 12,
        tone_step: 1,
        tones: vec![Tone { name: "bass".into(), value: 2 }, Tone { name: "treble".into(), value: -1 }],
        enhancer: true,
        bands: vec!["fm".into(), "dab".into()],
        band: "fm".into(),
        presets,
        au_format: "PCM".into(),
        au_fs: "44.1 kHz".into(),
        dlna_input: "server".into(),
        link_role: "server".into(),
        link_group_id: "9A237BF5AB80ED3C7251DFF49825CA42".into(),
        link_group_name: "Woonkamer".into(),
        link_clients: vec!["192.168.1.21".into()],
        ..Default::default()
    };
    let kitchen = Device {
        ip: "192.168.1.21".into(),
        name: "Keuken".into(),
        model: "WX-021".into(),
        alive: true,
        initialised: true,
        power_on: true,
        input: "mc_link".into(),
        volume: 22,
        volume_max: 60,
        volume_step: 1,
        link_role: "client".into(),
        link_group_id: "9A237BF5AB80ED3C7251DFF49825CA42".into(),
        ..Default::default()
    };
    let bedroom = Device {
        ip: "192.168.1.22".into(),
        name: "Slaapkamer".into(),
        model: "WX-010".into(),
        alive: true,
        initialised: true,
        volume_max: 60,
        ..Default::default()
    };
    let study = Device { ip: "192.168.1.30".into(), is_manual: true, ..Default::default() };
    sh.selected = living.ip.clone();
    sh.devices = vec![living, kitchen, bedroom, study];
    sh.searched = true;
    sh.play_clock = Some((83, Instant::now()));
}
