//! YamahaExtendedControl v1 HTTP API + UPnP description parsing.

use crate::model::*;
use serde_json::Value;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

pub const API: &str = "/YamahaExtendedControl/v1";

/// UDP port the device should send event notifications to (0 = not listening).
static EVENT_PORT: AtomicU16 = AtomicU16::new(0);

pub fn set_event_port(port: u16) {
    EVENT_PORT.store(port, Ordering::Relaxed);
}

/// One shared agent so connections to the device are reused.
pub fn agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| {
        ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(4))
            .user_agent(concat!("ControllerForMusicCast/", env!("CARGO_PKG_VERSION")))
            .build()
    })
}

/// GET a URL, return body text (None on any error).
pub fn get_text(url: &str, timeout: Duration) -> Option<String> {
    let resp = agent().get(url).timeout(timeout).call().ok()?;
    resp.into_string().ok()
}

fn with_event_headers(mut req: ureq::Request) -> ureq::Request {
    // Ask the device to push change notifications to us (valid for ~10 minutes,
    // renewed by every request that carries these headers).
    let port = EVENT_PORT.load(Ordering::Relaxed);
    if port != 0 {
        req = req.set("X-AppName", "MusicCast/1.0").set("X-AppPort", &port.to_string());
    }
    req
}

/// The device answers HTTP 200 even for errors; the real status is response_code.
fn check_response(resp: Result<ureq::Response, ureq::Error>) -> Result<Value, String> {
    let body = resp.map_err(|e| e.to_string())?.into_string().map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_str(&body).map_err(|e| e.to_string())?;
    match v.get("response_code").and_then(|x| x.as_i64()).unwrap_or(0) {
        0 => Ok(v),
        code => Err(response_code_text(code)),
    }
}

fn api_request(dev: &Device, path: &str, timeout: Duration) -> Result<Value, String> {
    let url = dev.api(&format!("{API}{path}"));
    check_response(with_event_headers(agent().get(&url).timeout(timeout)).call())
}

/// POST a JSON body to an API path (used by the dist/ MusicCast Link calls).
pub fn post_json(dev: &Device, path: &str, body: &Value) -> Result<(), String> {
    let url = dev.api(&format!("{API}{path}"));
    let req = with_event_headers(agent().post(&url).timeout(Duration::from_secs(4)));
    check_response(req.send_json(body)).map(|_| ())
}

fn response_code_text(code: i64) -> String {
    let what = match code {
        1 => "device is initialising",
        2 => "internal error",
        3 => "invalid request",
        4 => "invalid parameter",
        5 => "not possible in the current state",
        6 => "time-out",
        99 => "firmware is updating",
        100..=199 => "streaming service error",
        _ => "error",
    };
    format!("{what} (code {code})")
}

/// GET an API path on a device and parse JSON; None on transport or API error.
pub fn get_json(dev: &Device, path: &str) -> Option<Value> {
    api_request(dev, path, Duration::from_secs(4)).ok()
}

/// Command GET with a short timeout; Err carries a readable reason.
pub fn command(dev: &Device, path: &str) -> Result<(), String> {
    api_request(dev, path, Duration::from_millis(2500)).map(|_| ())
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn i(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_f64().map(|f| f as i64)).or_else(|| x.as_str().and_then(|t| t.parse().ok())))
        .unwrap_or(0)
}
fn b(v: &Value, key: &str) -> bool {
    v.get(key)
        .and_then(|x| x.as_bool().or_else(|| x.as_str().map(|t| t == "true" || t == "on")))
        .unwrap_or(false)
}
fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|x| x.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// <zone>/getStatus.
pub fn parse_status(dev: &mut Device, v: &Value) {
    if let Some(p) = v.get("power").and_then(|x| x.as_str()) {
        dev.power_on = p == "on";
    }
    if v.get("volume").is_some() {
        dev.volume = i(v, "volume");
    }
    if v.get("max_volume").is_some() {
        dev.volume_max = i(v, "max_volume");
    }
    dev.volume_db = v.get("actual_volume").and_then(|a| {
        let value = a.get("value")?.as_f64()?;
        let unit = s(a, "unit");
        (unit == "dB").then_some((value, unit))
    });
    if v.get("input").is_some() {
        dev.input = s(v, "input");
    }
    if v.get("mute").is_some() {
        dev.muted = b(v, "mute");
    }
    if v.get("sound_program").is_some() {
        dev.sound_program = s(v, "sound_program");
    }
    if v.get("direct").is_some() {
        dev.direct = b(v, "direct");
    }
    if v.get("enhancer").is_some() {
        dev.enhancer = b(v, "enhancer");
    }
    if v.get("bass_extension").is_some() {
        dev.bass_extension = b(v, "bass_extension");
    }
    if v.get("clear_voice").is_some() {
        dev.clear_voice = b(v, "clear_voice");
    }
    // tone_control (bass/treble) takes precedence over the equalizer (low/mid/high).
    let tone = v.get("tone_control").and_then(|x| x.as_object()).map(|o| (ToneKind::ToneControl, o));
    let eq = v.get("equalizer").and_then(|x| x.as_object()).map(|o| (ToneKind::Equalizer, o));
    if let Some((kind, obj)) = tone.or(eq) {
        dev.tone_kind = kind;
        dev.tones = obj
            .iter()
            .filter(|(k, _)| k.as_str() != "mode")
            .map(|(k, val)| Tone { name: k.clone(), value: val.as_i64().unwrap_or(0) })
            .collect();
    }
}

/// netusb/getPlayInfo. Does not touch `dev.input`: the netusb block reports its
/// own last source even when the zone is on, say, optical.
pub fn parse_play_info(dev: &mut Device, v: &Value) {
    if let Some(pb) = v.get("playback").and_then(|x| x.as_str()) {
        dev.playing = pb == "play";
        dev.paused = pb == "pause";
        dev.stopped = pb == "stop";
    }
    if v.get("repeat").is_some() {
        dev.repeat = s(v, "repeat");
    }
    if let Some(sh) = v.get("shuffle") {
        // String ("off"/"on"/"songs"/"albums") on current firmware, bool on old.
        dev.shuffle = match sh {
            Value::Bool(true) => "on".into(),
            Value::Bool(false) => "off".into(),
            Value::String(t) => t.clone(),
            _ => "off".into(),
        };
    }
    dev.repeat_modes = available_modes(v.get("repeat_available"), &["off", "one", "all"]);
    dev.shuffle_modes = available_modes(v.get("shuffle_available"), &["off", "on"]);
    dev.track = percent_decode(&s(v, "track"));
    dev.artist = percent_decode(&s(v, "artist"));
    dev.album = percent_decode(&s(v, "album"));
    dev.total_time = i(v, "total_time");
    dev.play_time = i(v, "play_time");
    let art = s(v, "albumart_url");
    let base = if art.is_empty() {
        String::new()
    } else if art.starts_with("http") {
        art
    } else {
        format!("http://{}{}", dev.ip, art)
    };
    // Some sources reuse one URL for every track; albumart_id changes with the image.
    let id = i(v, "albumart_id");
    dev.art_url = if base.is_empty() || id == 0 { base } else { format!("{base}#{id}") };
}

/// `*_available` is an array of modes on current firmware, a bool on older ones,
/// and missing on the oldest (then assume all modes work).
fn available_modes(v: Option<&Value>, all: &[&str]) -> Vec<String> {
    match v {
        Some(Value::Array(a)) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
        Some(Value::Bool(false)) => Vec::new(),
        _ => all.iter().map(|s| s.to_string()).collect(),
    }
}

/// tuner/getPlayInfo: frequency and station info live in per-band objects.
pub fn parse_tuner_info(dev: &mut Device, v: &Value) {
    if v.get("band").is_some() {
        dev.band = s(v, "band");
    }
    let band = dev.band.to_lowercase();
    dev.tuner_station.clear();
    dev.tuner_text.clear();
    if let Some(obj) = v.get(band.as_str()) {
        if obj.get("freq").is_some() {
            dev.tuner_freq = i(obj, "freq");
        }
    }
    match band.as_str() {
        "fm" => {
            if let Some(rds) = v.get("rds") {
                dev.tuner_station = s(rds, "program_service").trim().to_string();
                let a = s(rds, "radio_text_a");
                dev.tuner_text = if a.trim().is_empty() { s(rds, "radio_text_b") } else { a }.trim().to_string();
            }
        }
        "dab" => {
            if let Some(dab) = v.get("dab") {
                dev.tuner_station = s(dab, "service_label").trim().to_string();
                dev.tuner_text = s(dab, "dls").trim().to_string();
            }
        }
        _ => {}
    }
}

/// <zone>/getSignalInfo.
pub fn parse_signal_info(dev: &mut Device, v: &Value) {
    if let Some(audio) = v.get("audio") {
        dev.au_format = s(audio, "format");
        dev.au_fs = s(audio, "fs");
    }
}

/// system/getFuncStatus — dimmer value.
pub fn parse_func_status(dev: &mut Device, v: &Value) {
    if v.get("dimmer").is_some() {
        dev.dimmer = i(v, "dimmer");
    }
}

fn push_inputs(dev: &mut Device, list: &[Value]) {
    for item in list {
        let id = s(item, "id");
        if id.is_empty() || dev.inputs.iter().any(|e| e.id == id) {
            continue;
        }
        if id.eq_ignore_ascii_case("server") {
            dev.dlna_input = id.clone();
        }
        let text = s(item, "text");
        dev.inputs.push(InputItem {
            title: if text.is_empty() { input_display_name(&id) } else { text },
            play_info_type: s(item, "play_info_type"),
            id,
        });
    }
}

/// system/getFeatures, for the device's current zone.
pub fn parse_features(dev: &mut Device, v: &Value) {
    dev.initialised = true;
    dev.inputs.clear();
    if let Some(sys) = v.get("system") {
        if let Some(list) = sys.get("input_list").and_then(|x| x.as_array()) {
            push_inputs(dev, list);
        }
        dev.sys_func_list = str_list(sys.get("func_list"));
        if let Some(rs) = sys.get("range_step").and_then(|x| x.as_array()) {
            for item in rs.iter().filter(|it| s(it, "id") == "dimmer") {
                dev.dim_max = i(item, "max");
                dev.dim_min = i(item, "min");
                dev.dim_step = i(item, "step").max(1);
            }
        }
    }
    if let Some(tuner) = v.get("tuner") {
        let fl = str_list(tuner.get("func_list"));
        dev.bands = fl.iter().filter(|b| matches!(b.to_lowercase().as_str(), "fm" | "am" | "dab")).cloned().collect();
        if let Some(rs) = tuner.get("range_step").and_then(|x| x.as_array()) {
            for item in rs {
                let id = s(item, "id");
                if !id.is_empty() {
                    dev.band_ranges.insert(
                        id,
                        BandRange { min: i(item, "min"), max: i(item, "max"), step: i(item, "step").max(1) },
                    );
                }
            }
        }
        dev.tuner_preset_type = tuner.get("preset").map(|p| s(p, "type")).unwrap_or_default();
        if dev.band.is_empty() {
            dev.band = dev.bands.first().cloned().unwrap_or_default();
        }
    }
    if let Some(zones) = v.get("zone").and_then(|x| x.as_array()) {
        dev.zones = zones.iter().map(|z| s(z, "id")).filter(|id| !id.is_empty()).collect();
        if !dev.zones.contains(&dev.zone) {
            dev.zone = dev.zones.first().cloned().unwrap_or_else(|| "main".into());
        }
        // Everything below is per zone: reset so a zone switch doesn't mix features.
        dev.func_list.clear();
        dev.programs.clear();
        dev.volume_min = 0;
        dev.volume_max = 0;
        dev.volume_step = 0;
        dev.tone_min = 0;
        dev.tone_max = 0;
        dev.tone_step = 0;
        if let Some(z) = zones.iter().find(|z| s(z, "id") == dev.zone) {
            dev.func_list = str_list(z.get("func_list"));
            let has_tone = dev.func_list.iter().any(|f| f == "tone_control");
            if let Some(rs) = z.get("range_step").and_then(|x| x.as_array()) {
                for item in rs {
                    match s(item, "id").as_str() {
                        "volume" => {
                            dev.volume_max = i(item, "max");
                            dev.volume_min = i(item, "min");
                            dev.volume_step = i(item, "step").max(1);
                        }
                        id @ ("tone_control" | "equalizer") if id == "tone_control" || !has_tone => {
                            dev.tone_max = i(item, "max");
                            dev.tone_min = i(item, "min");
                            dev.tone_step = i(item, "step").max(1);
                        }
                        _ => {}
                    }
                }
            }
            if let Some(list) = z.get("input_list").and_then(|x| x.as_array()) {
                push_inputs(dev, list);
            }
            dev.programs = str_list(z.get("sound_program_list"));
        }
    }
    // Fallbacks (estimated) when the device reports nothing.
    if dev.volume_max <= 0 {
        dev.volume_max = 100;
    }
    if dev.volume_step <= 0 {
        dev.volume_step = 1;
    }
    if dev.tone_max == 0 && dev.tone_min == 0 {
        dev.tone_min = -10;
        dev.tone_max = 10;
    }
    if dev.tone_step <= 0 {
        dev.tone_step = 1;
    }
    if dev.dim_step <= 0 {
        dev.dim_step = 1;
    }
}

/// system/getNameText: user-renamed inputs replace the default titles.
pub fn parse_name_text(dev: &mut Device, v: &Value) {
    if let Some(list) = v.get("input_list").and_then(|x| x.as_array()) {
        for item in list {
            let (id, text) = (s(item, "id"), s(item, "text"));
            if let Some(input) = dev.inputs.iter_mut().find(|i| i.id == id) {
                if !text.trim().is_empty() {
                    input.title = text.trim().to_string();
                }
            }
        }
    }
}

/// dist/getDistributionInfo.
pub fn parse_distribution(dev: &mut Device, v: &Value) {
    dev.link_role = s(v, "role");
    dev.link_group_id = s(v, "group_id");
    dev.link_group_name = s(v, "group_name");
    dev.link_clients = v
        .get("client_list")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|c| s(c, "ip_address")).filter(|ip| !ip.is_empty()).collect())
        .unwrap_or_default();
}

/// <zone>/getSoundProgramList.
pub fn parse_programs(dev: &mut Device, v: &Value) {
    let progs = str_list(v.get("sound_program_list"));
    if !progs.is_empty() {
        dev.programs = progs;
    }
}

/// preset_info arrays (netusb/getPresetInfo, tuner/getPresetInfo?band=).
pub fn parse_presets(v: &Value, band: &str) -> Vec<Preset> {
    let mut out = Vec::new();
    if let Some(list) = v.get("preset_info").and_then(|x| x.as_array()) {
        for (idx, item) in list.iter().enumerate() {
            let reported_num = i(item, "number");
            let num = if reported_num > 0 { reported_num } else { (idx + 1) as i64 };
            // Tuner entries are band-specific; netusb entries carry "input".
            let reported_band = s(item, "band");
            let b = if reported_band.is_empty() || reported_band.eq_ignore_ascii_case("unknown") {
                band.to_string()
            } else {
                reported_band
            };
            out.push(Preset { num, band: b, text: s(item, "text") });
        }
    }
    out
}

/// Result of netusb/getListInfo.
pub struct ListPage {
    pub entries: Vec<ListEntry>,
    pub index: i64,
    pub max_line: i64,
    pub layer: i64,
    pub name: String,
}

pub fn parse_list(v: &Value) -> ListPage {
    let entries = v
        .get("list_info")
        .and_then(|x| x.as_array())
        .map(|a| a.iter().map(|item| ListEntry { text: s(item, "text"), attribute: i(item, "attribute") }).collect())
        .unwrap_or_default();
    ListPage {
        entries,
        index: i(v, "index"),
        max_line: i(v, "max_line"),
        layer: i(v, "menu_layer"),
        name: s(v, "menu_name"),
    }
}

/// Fetch the UPnP device description and build a Device if it is a Yamaha
/// MediaRenderer.
pub fn device_from_description(desc_url: &str, manual: bool) -> Option<Device> {
    let body = get_text(desc_url, Duration::from_secs(3))?;
    parse_description(&body, desc_url, manual).ok()
}

pub fn parse_description(body: &str, desc_url: &str, manual: bool) -> Result<Device, String> {
    let doc = roxmltree::Document::parse(body).map_err(|e| e.to_string())?;
    let root = doc.root_element();
    let find = |name: &str| -> String {
        root.descendants()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    if !find("deviceType").to_lowercase().contains("mediarenderer") {
        return Err("not a media renderer".into());
    }
    let friendly = find("friendlyName");
    let model = find("modelName");
    let manufacturer = find("manufacturer");
    let hay = format!("{friendly} {model} {manufacturer}").to_lowercase();
    if !hay.contains("yamaha") {
        return Err("not a MusicCast device".into());
    }
    // Base URL from the description URL (scheme://host:port) for UPnP/DLNA/icons.
    let base = base_url_of(desc_url);
    let ip = host_of(desc_url);
    // The ExtendedControl API lives on the plain HTTP port (80), not the UPnP
    // description port (49154).
    let scheme = desc_url.split("://").next().unwrap_or("http");
    let api_base = format!("{scheme}://{ip}");
    let abs = |u: &str| -> String {
        if u.starts_with("http") {
            u.to_string()
        } else if u.starts_with('/') {
            format!("{base}{u}")
        } else {
            format!("{base}/{u}")
        }
    };
    // AVTransport control URL from the service list; MusicCast default /AVTransport/ctrl.
    let mut dlna_url = format!("{base}/AVTransport/ctrl");
    for svc in root.descendants().filter(|n| n.has_tag_name("service")) {
        let st = svc.children().find(|n| n.has_tag_name("serviceType")).and_then(|n| n.text()).unwrap_or("");
        if st.contains("AVTransport") {
            if let Some(ctrl) = svc.children().find(|n| n.has_tag_name("controlURL")).and_then(|n| n.text()) {
                dlna_url = abs(ctrl.trim());
            }
        }
    }
    // Largest icon.
    let mut icon_url = String::new();
    let mut best = -1i64;
    for icon in root.descendants().filter(|n| n.has_tag_name("icon")) {
        let w = icon
            .children()
            .find(|n| n.has_tag_name("width"))
            .and_then(|n| n.text())
            .and_then(|t| t.trim().parse::<i64>().ok())
            .unwrap_or(0);
        if let Some(url) = icon.children().find(|n| n.has_tag_name("url")).and_then(|n| n.text()) {
            if w > best {
                best = w;
                icon_url = abs(url.trim());
            }
        }
    }
    Ok(Device {
        ip,
        base_url: api_base,
        name: friendly,
        model,
        serial: find("serialNumber"),
        icon_url,
        dlna_url,
        dlna_input: "server".into(),
        is_manual: manual,
        alive: true,
        zone: "main".into(),
        volume_max: 100,
        volume_step: 1,
        tone_min: -10,
        tone_max: 10,
        tone_step: 1,
        dim_step: 1,
        ..Default::default()
    })
}

pub fn base_url_of(url: &str) -> String {
    if let Some(pos) = url.find("://") {
        let rest = &url[pos + 3..];
        let end = rest.find('/').unwrap_or(rest.len());
        format!("{}://{}", &url[..pos], &rest[..end])
    } else {
        url.to_string()
    }
}

pub fn host_of(url: &str) -> String {
    let base = base_url_of(url);
    let after = base.split("://").nth(1).unwrap_or(&base);
    after.split(':').next().unwrap_or(after).to_string()
}

/// Percent-decoding used for track/artist/album and served file paths.
pub fn percent_decode(s: &str) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(a), Some(b2)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push(a * 16 + b2);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Probe http://<host>:49154/MediaRenderer/desc.xml with a 5 s timeout.
pub fn test_manual_ip(host: &str) -> Result<Device, String> {
    let url = format!("http://{}:49154/MediaRenderer/desc.xml", host.trim());
    let body = agent()
        .get(&url)
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| e.to_string())?
        .into_string()
        .map_err(|e| e.to_string())?;
    parse_description(&body, &url, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn percent_decode_trailing_escape() {
        assert_eq!(percent_decode("100%25"), "100%");
        assert_eq!(percent_decode("a%20b"), "a b");
        assert_eq!(percent_decode("50%"), "50%");
        assert_eq!(percent_decode("5%2"), "5%2");
        assert_eq!(percent_decode("100% Hits"), "100% Hits");
    }

    #[test]
    fn tuner_freq_is_read_per_band() {
        let mut d = Device::default();
        parse_tuner_info(
            &mut d,
            &json!({"response_code":0,"band":"fm","fm":{"preset":1,"freq":98700,"tuned":true},
                    "am":{"freq":1000},"rds":{"program_service":"KINK    ","radio_text_a":"Now: song"}}),
        );
        assert_eq!(d.tuner_freq, 98700);
        assert_eq!(d.tuner_station, "KINK");
        assert_eq!(d.tuner_text, "Now: song");
    }

    #[test]
    fn play_info_keeps_input_and_reads_modes() {
        let mut d = Device { input: "optical".into(), ..Default::default() };
        parse_play_info(
            &mut d,
            &json!({"input":"spotify","playback":"play","repeat":"all","shuffle":"on",
                    "repeat_available":["off","all"],"shuffle_available":["off","on"],
                    "albumart_url":"/YamahaRemoteControl/AlbumART/AlbumART.jpg","albumart_id":42}),
        );
        assert_eq!(d.input, "optical");
        assert!(d.playing);
        assert_eq!(d.shuffle, "on");
        assert_eq!(d.repeat_modes, vec!["off", "all"]);
        assert!(d.art_url.ends_with("AlbumART.jpg#42"));
    }

    #[test]
    fn distribution_info() {
        let mut d = Device::default();
        parse_distribution(
            &mut d,
            &json!({"response_code":0,"group_id":"9A237BF5AB80ED3C7251DFF49825CA42","group_name":"Woonkamer",
                    "role":"server","server_zone":"main",
                    "client_list":[{"ip_address":"192.168.1.21","data_type":"base"}]}),
        );
        assert!(d.is_link_server());
        assert_eq!(d.link_clients, vec!["192.168.1.21"]);
        parse_distribution(&mut d, &json!({"group_id":"00000000000000000000000000000000","role":"none","client_list":[]}));
        assert!(!d.is_link_server() && !d.is_link_client());
    }

    #[test]
    fn equalizer_is_recognised() {
        let mut d = Device::default();
        parse_status(&mut d, &json!({"equalizer":{"mode":"manual","low":1,"mid":0,"high":-2}}));
        assert_eq!(d.tone_kind, ToneKind::Equalizer);
        assert_eq!(d.tones.len(), 3);
    }

    #[test]
    fn features_are_per_zone() {
        let mut d = Device { zone: "zone2".into(), func_list: vec!["stale".into()], ..Default::default() };
        parse_features(
            &mut d,
            &json!({"system":{"input_list":[{"id":"spotify","play_info_type":"netusb"},{"id":"optical","play_info_type":"none"}]},
                    "zone":[{"id":"main","func_list":["power","enhancer"],"range_step":[{"id":"volume","min":0,"max":161,"step":1}]},
                            {"id":"zone2","func_list":["power"],"range_step":[{"id":"volume","min":0,"max":60,"step":1}]}]}),
        );
        assert_eq!(d.func_list, vec!["power"]);
        assert_eq!(d.volume_max, 60);
        assert_eq!(d.inputs.len(), 2);
        assert_eq!(d.inputs[1].play_info_type, "none");
    }
}
