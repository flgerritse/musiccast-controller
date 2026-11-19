//! YamahaExtendedControl v1 HTTP API + UPnP description parsing.

use crate::model::*;
use serde_json::Value;
use std::time::Duration;

pub const API: &str = "/YamahaExtendedControl/v1";

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(4))
        .user_agent("john/1.0")
        .build()
}

/// GET a URL, return body text (None on any error).
pub fn get_text(url: &str) -> Option<String> {
    let resp = agent().get(url).call().ok()?;
    resp.into_string().ok()
}

/// GET an API path on a device and parse JSON, tolerating a missing/zero
/// response_code.
pub fn get_json(dev: &Device, path: &str) -> Option<Value> {
    let url = dev.api(&format!("{}{}", API, path));
    let body = get_text(&url)?;
    let v: Value = serde_json::from_str(&body).ok()?;
    Some(v)
}

/// Fire-and-forget command GET with a 2 s timeout.
pub fn command(dev: &Device, path: &str) -> bool {
    let url = dev.api(&format!("{}{}", API, path));
    let agent = ureq::AgentBuilder::new()
        .timeout(Duration::from_millis(2000))
        .user_agent("john/1.0")
        .build();
    agent.get(&url).call().is_ok()
}

fn s(v: &Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}
fn i(v: &Value, key: &str) -> i64 {
    v.get(key)
        .and_then(|x| x.as_i64().or_else(|| x.as_str().and_then(|t| t.parse().ok())))
        .unwrap_or(0)
}
fn b(v: &Value, key: &str) -> bool {
    v.get(key)
        .and_then(|x| x.as_bool().or_else(|| x.as_str().map(|t| t == "true")))
        .unwrap_or(false)
}

/// main/getStatus.
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
    if v.get("input").is_some() {
        dev.input = s(v, "input");
    }
    if v.get("mute").is_some() {
        dev.muted = b(v, "mute");
    }
    if v.get("sound_program").is_some() {
        dev.sound_program = s(v, "sound_program");
    }
    if v.get("surr_decoder_type").is_some() {
        dev.surr_decode = s(v, "surr_decoder_type");
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
    if v.get("party_enable").is_some() {
        dev.party = b(v, "party_enable");
    }
    if v.get("contents_display").is_some() {
        dev.contents_display = b(v, "contents_display");
    }
    // tone_control / Equalizer dicts: keys other than "mode" become tone rows.
    for (key, type_name) in [("tone_control", "ToneControl"), ("equalizer", "Equalizer")] {
        if let Some(tc) = v.get(key).and_then(|x| x.as_object()) {
            dev.tone_type = type_name.to_string();
            dev.tones.clear();
            for (k, val) in tc {
                if k == "mode" {
                    dev.tone_mode = val.as_str().unwrap_or("").to_string();
                } else {
                    dev.tones.push(Tone {
                        name: k.clone(),
                        value: val.as_i64().unwrap_or(0),
                    });
                }
            }
        }
    }
}

/// netusb/getPlayInfo.
pub fn parse_play_info(dev: &mut Device, v: &Value) {
    if v.get("input").is_some() {
        dev.input = s(v, "input");
    }
    if let Some(pb) = v.get("playback").and_then(|x| x.as_str()) {
        dev.playback = pb.to_string();
        dev.playing = pb == "play";
        dev.stopped = pb == "stop";
    }
    if v.get("repeat").is_some() {
        dev.repeat = s(v, "repeat");
    }
    if v.get("shuffle").is_some() {
        dev.shuffle = b(v, "shuffle");
    }
    dev.track = percent_decode(&s(v, "track"));
    dev.artist = percent_decode(&s(v, "artist"));
    dev.album = percent_decode(&s(v, "album"));
    dev.total_time = i(v, "total_time");
    dev.play_time = i(v, "play_time");
    let art = s(v, "albumart_url");
    dev.art_url = if art.is_empty() {
        String::new()
    } else if art.starts_with("http") {
        art
    } else {
        // relative path: prefix with the device address
        format!("http://{}{}", dev.ip, art)
    };
    if v.get("repeat_available").is_some() {
        dev.repeat_available = b(v, "repeat_available");
    }
    if v.get("shuffle_available").is_some() {
        dev.shuffle_available = b(v, "shuffle_available");
    }
}

/// tuner/getPlayInfo.
pub fn parse_tuner_info(dev: &mut Device, v: &Value) {
    if v.get("band").is_some() {
        dev.band = s(v, "band");
    }
    if v.get("freq").is_some() {
        dev.tuner_freq = i(v, "freq");
    }
    if let Some(preset) = v.get("preset") {
        if preset.is_object() {
            // some firmwares report the current preset number here
            let _ = i(preset, "num");
        }
    }
}

/// main/getSignalInfo.
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

/// system/getFeatures.
pub fn parse_features(dev: &mut Device, v: &Value) {
    if let Some(sys) = v.get("system") {
        // system.input_list -> terminals; id "server" is the DLNA input.
        if let Some(list) = sys.get("input_list").and_then(|x| x.as_array()) {
            dev.inputs.clear();
            for item in list {
                let id = s(item, "id");
                if id.is_empty() {
                    continue;
                }
                if id.to_lowercase() == "server" {
                    dev.dlna_input = id.clone();
                }
                let title = s(item, "text");
                dev.inputs.push(InputItem {
                    title: if title.is_empty() { id.clone() } else { title },
                    id,
                });
            }
        }
        if let Some(fl) = sys.get("func_list").and_then(|x| x.as_array()) {
            dev.sys_func_list = fl.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        }
        // system.range_step: id "dimmer" -> min/max/step.
        if let Some(rs) = sys.get("range_step").and_then(|x| x.as_array()) {
            for item in rs {
                if s(item, "id") == "dimmer" {
                    dev.dim_max = i(item, "max");
                    dev.dim_min = i(item, "min");
                    dev.dim_step = i(item, "step").max(1);
                }
            }
        }
    }
    if let Some(tuner) = v.get("tuner") {
        if let Some(fl) = tuner.get("func_list").and_then(|x| x.as_array()) {
            dev.bands = fl
                .iter()
                .filter_map(|x| x.as_str().map(String::from))
                .filter(|b| matches!(b.to_lowercase().as_str(), "fm" | "am" | "dab" | "fm_hd" | "hd"))
                .collect();
            if dev.bands.is_empty() {
                dev.bands = fl.iter().filter_map(|x| x.as_str().map(String::from)).collect();
            }
        }
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
        if dev.band.is_empty() {
            dev.band = dev.bands.first().cloned().unwrap_or_default();
        }
    }
    if let Some(zones) = v.get("zone").and_then(|x| x.as_array()) {
        dev.zones.clear();
        for z in zones {
            let id = s(z, "id");
            if !id.is_empty() && !dev.zones.contains(&id) {
                dev.zones.push(id.clone());
            }
            if dev.zone.is_empty() {
                dev.zone = id.clone();
            }
            if id == dev.zone || dev.zone.is_empty() {
                if let Some(fl) = z.get("func_list").and_then(|x| x.as_array()) {
                    for f in fl.iter().filter_map(|x| x.as_str()) {
                        if !dev.func_list.iter().any(|e| e == f) {
                            dev.func_list.push(f.to_string());
                        }
                    }
                }
                if let Some(rs) = z.get("range_step").and_then(|x| x.as_array()) {
                    for item in rs {
                        match s(item, "id").as_str() {
                            "volume" => {
                                dev.volume_max = i(item, "max");
                                dev.volume_min = i(item, "min");
                                dev.volume_step = i(item, "step").max(1);
                            }
                            "tone_control" | "equalizer" => {
                                dev.tone_max = i(item, "max");
                                dev.tone_min = i(item, "min");
                                dev.tone_step = i(item, "step").max(1);
                            }
                            _ => {}
                        }
                    }
                }
                // zone input_list (newer firmware) merges into terminals.
                if let Some(list) = z.get("input_list").and_then(|x| x.as_array()) {
                    for item in list {
                        let id = s(item, "id");
                        if id.is_empty() || dev.inputs.iter().any(|e| e.id == id) {
                            continue;
                        }
                        if id.to_lowercase() == "server" {
                            dev.dlna_input = id.clone();
                        }
                        let title = s(item, "text");
                        dev.inputs.push(InputItem {
                            title: if title.is_empty() { id.clone() } else { title },
                            id,
                        });
                    }
                }
                if let Some(sp) = z.get("sound_program_list").and_then(|x| x.as_array()) {
                    if dev.programs.is_empty() {
                        dev.programs = sp.iter().filter_map(|x| x.as_str().map(String::from)).collect();
                    }
                }
            }
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

/// main/getSoundProgramList.
pub fn parse_programs(dev: &mut Device, v: &Value) {
    if let Some(list) = v.get("sound_program_list").and_then(|x| x.as_array()) {
        let progs: Vec<String> = list.iter().filter_map(|x| x.as_str().map(String::from)).collect();
        if !progs.is_empty() {
            dev.programs = progs;
        }
    }
}

/// preset_info arrays (netusb/getPresetInfo, tuner/getPresetInfo?band=).
pub fn parse_presets(v: &Value, band: &str) -> Vec<Preset> {
    let mut out = Vec::new();
    if let Some(list) = v.get("preset_info").and_then(|x| x.as_array()) {
        for (idx, item) in list.iter().enumerate() {
            let reported_num = i(item, "number");
            let num = if reported_num > 0 { reported_num } else { (idx + 1) as i64 };
            let text = s(item, "text");
            let reported_band = s(item, "band");
            // Empty tuner slots can report "unknown" rather than their owning band.
            let b = if reported_band.is_empty() || reported_band.eq_ignore_ascii_case("unknown") { band.to_string() } else { reported_band };
            out.push(Preset { num, band: b, text });
        }
    }
    out
}

/// netusb/getListInfo.
pub fn parse_list(v: &Value) -> (Vec<ListEntry>, i64, i64) {
    let mut out = Vec::new();
    let mut max_line = 0;
    let mut menu_layer = 0;
    if let Some(list) = v.get("list_info").and_then(|x| x.as_array()) {
        for item in list {
            out.push(ListEntry {
                text: s(item, "text"),
                attribute: i(item, "attribute"),
            });
        }
    }
    if let Some(ml) = v.get("max_line").and_then(|x| x.as_i64()) {
        max_line = ml;
    }
    if let Some(ml) = v.get("menu_layer").and_then(|x| x.as_i64()) {
        menu_layer = ml;
    }
    (out, max_line, menu_layer)
}

/// Fetch the UPnP device description and build a Device if it is a Yamaha
/// MediaRenderer.
pub fn device_from_description(desc_url: &str, manual: bool) -> Option<Device> {
    let body = get_text(desc_url)?;
    let doc = roxmltree::Document::parse(&body).ok()?;
    let root = doc.root_element();
    let find = |name: &str| -> String {
        root.descendants()
            .find(|n| n.has_tag_name(name))
            .and_then(|n| n.text())
            .unwrap_or("")
            .trim()
            .to_string()
    };
    let device_type = find("deviceType").to_lowercase();
    if !device_type.contains("mediarenderer") {
        return None;
    }
    let friendly = find("friendlyName");
    let model = find("modelName");
    let manufacturer = find("manufacturer");
    let hay = format!("{} {} {}", friendly, model, manufacturer).to_lowercase();
    if !hay.contains("yamaha") {
        return None;
    }
    // Base URL from the description URL (scheme://host:port) for UPnP/DLNA/icons.
    let base = base_url_of(desc_url);
    let ip = host_of(desc_url);
    // The ExtendedControl API lives on the plain HTTP port (80), not the UPnP
    // description port (49154).
    let scheme = desc_url.split("://").next().unwrap_or("http");
    let api_base = format!("{}://{}", scheme, ip);
    // AVTransport control URL from the service list; MusicCast default /AVTransport/ctrl.
    let mut dlna_url = format!("{}/AVTransport/ctrl", base);
    for svc in root.descendants().filter(|n| n.has_tag_name("service")) {
        let st = svc.children().find(|n| n.has_tag_name("serviceType")).and_then(|n| n.text()).unwrap_or("");
        if st.contains("AVTransport") {
            if let Some(ctrl) = svc.children().find(|n| n.has_tag_name("controlURL")).and_then(|n| n.text()) {
                dlna_url = if ctrl.starts_with("http") {
                    ctrl.to_string()
                } else {
                    format!("{}{}", base, if ctrl.starts_with('/') { ctrl.to_string() } else { format!("/{}", ctrl) })
                };
            }
        }
    }
    // Largest icon.
    let mut icon_url = String::new();
    let mut best = 0i64;
    for icon in root.descendants().filter(|n| n.has_tag_name("icon")) {
        let w = icon.children().find(|n| n.has_tag_name("width")).and_then(|n| n.text()).and_then(|t| t.parse::<i64>().ok()).unwrap_or(0);
        if let Some(url) = icon.children().find(|n| n.has_tag_name("url")).and_then(|n| n.text()) {
            if w >= best {
                best = w;
                icon_url = if url.starts_with("http") { url.to_string() } else { format!("{}{}", base, url) };
            }
        }
    }
    Some(Device {
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

/// Percent-decoding used for track/artist/album (stringByRemovingPercentEncoding).
pub fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let h = |c: u8| -> Option<u8> {
                match c {
                    b'0'..=b'9' => Some(c - b'0'),
                    b'a'..=b'f' => Some(c - b'a' + 10),
                    b'A'..=b'F' => Some(c - b'A' + 10),
                    _ => None,
                }
            };
            if let (Some(a), Some(b2)) = (h(bytes[i + 1]), h(bytes[i + 2])) {
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

/// Probe http://<ip>:49154/MediaRenderer/desc.xml with a 5 s timeout.
pub fn test_manual_ip(ip: &str) -> Result<Device, String> {
    let url = format!("http://{}:49154/MediaRenderer/desc.xml", ip.trim());
    let resp = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(5))
        .build()
        .get(&url)
        .call();
    match resp {
        Ok(r) => {
            let body = r.into_string().map_err(|e| e.to_string())?;
            let _ = body;
            device_from_description(&url, true).ok_or_else(|| "Not a MusicCast device".to_string())
        }
        Err(e) => Err(e.to_string()),
    }
}