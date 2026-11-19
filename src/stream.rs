//! Local music streaming: a small HTTP file server (port 9999, +10 retry up to
//! 100 times) plus UPnP AVTransport push (SetAVTransportURI + Play, DIDL-Lite
//! metadata).

use crate::api;
use std::io::{Read, Seek, SeekFrom};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub const SUPPORTED_EXTS: [&str; 11] =
    ["mp3", "wma", "aac", "ogg", "flac", "alac", "aiff", "wav", "mp4", "ape", "m4a"];

pub fn is_supported(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| SUPPORTED_EXTS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// MIME type by file extension (default application/octet-stream).
pub fn mime_for(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).unwrap_or("").to_lowercase().as_str() {
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "mp3" => "audio/mpeg",
        "dsf" => "application/octet-stream",
        "m4a" => "audio/mp4",
        "wma" => "audio/x-ms-wma",
        "m3u8" => "application/x-mpegURL",
        "ts" => "video/MP2T",
        _ => "application/octet-stream",
    }
}

pub fn local_ip() -> String {
    // UDP "connect" trick: picks the LAN source address without sending traffic.
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = sock.local_addr() {
                return addr.ip().to_string();
            }
        }
    }
    "127.0.0.1".to_string()
}

pub fn url_encode_path(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub struct FileServer {
    pub port: u16,
    files: Arc<Mutex<Vec<PathBuf>>>,
}

impl FileServer {
    /// Start on 9999, retrying port+10 up to 100 times.
    pub fn start() -> Option<FileServer> {
        for attempt in 0..100u16 {
            let port = 9999 + attempt * 10;
            if let Ok(server) = Server::http(("0.0.0.0", port)) {
                let files: Arc<Mutex<Vec<PathBuf>>> = Arc::new(Mutex::new(Vec::new()));
                let f2 = files.clone();
                std::thread::spawn(move || serve_loop(server, f2));
                return Some(FileServer { port, files });
            }
        }
        None
    }

    pub fn set_files(&self, files: Vec<PathBuf>) {
        *self.files.lock().unwrap() = files;
    }

    pub fn url_for(&self, path: &Path) -> String {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        format!("http://{}:{}/{}", local_ip(), self.port, url_encode_path(name))
    }
}

fn serve_loop(server: Server, files: Arc<Mutex<Vec<PathBuf>>>) {
    for request in server.incoming_requests() {
        let url_path = request.url().split('?').next().unwrap_or("/").to_string();
        let name = api::percent_decode(url_path.trim_start_matches('/'));
        let found = files
            .lock()
            .unwrap()
            .iter()
            .find(|p| p.file_name().and_then(|n| n.to_str()).map(|n| n == name).unwrap_or(false))
            .cloned();
        let Some(path) = found else {
            let _ = request.respond(Response::from_string("not found").with_status_code(StatusCode(404)));
            continue;
        };
        if request.method() != &Method::Get && request.method() != &Method::Head {
            let _ = request.respond(Response::from_string("method not allowed").with_status_code(StatusCode(405)));
            continue;
        }
        let Ok(mut file) = std::fs::File::open(&path) else {
            let _ = request.respond(Response::from_string("open failed").with_status_code(StatusCode(500)));
            continue;
        };
        let total = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mime = mime_for(&path);
        let ctype = Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()).unwrap();
        let accept = Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap();

        // Single-range support (devices may request chunks).
        let range = request
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("Range"))
            .map(|h| h.value.as_str().to_string());
        if let Some(range) = range {
            if let Some((start, end)) = parse_range(&range, total) {
                let len = end - start + 1;
                let mut buf = vec![0u8; len as usize];
                if file.seek(SeekFrom::Start(start)).is_ok() && file.read_exact(&mut buf).is_ok() {
                    let cr = Header::from_bytes(
                        &b"Content-Range"[..],
                        format!("bytes {}-{}/{}", start, end, total).as_bytes(),
                    )
                    .unwrap();
                    let resp = Response::from_data(buf)
                        .with_status_code(StatusCode(206))
                        .with_header(ctype)
                        .with_header(accept)
                        .with_header(cr);
                    let _ = request.respond(resp);
                    continue;
                }
            }
        }
        let resp = Response::from_file(file)
            .with_header(ctype)
            .with_header(accept);
        let _ = request.respond(resp);
    }
}

fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let v = value.trim().strip_prefix("bytes=")?;
    let (a, b) = v.split_once('-')?;
    if total == 0 {
        return None;
    }
    if a.is_empty() {
        // suffix range: last N bytes
        let n: u64 = b.parse().ok()?;
        let start = total.saturating_sub(n);
        Some((start, total - 1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end: u64 = if b.is_empty() { total - 1 } else { b.parse().ok()? };
        if start >= total {
            return None;
        }
        Some((start, end.min(total - 1)))
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

const AV_SERVICE: &str = "urn:schemas-upnp-org:service:AVTransport:1";

fn soap(ctrl_url: &str, action: &str, args: &str) -> Result<(), String> {
    let body = format!(
        "<s:Envelope xmlns:s='http://schemas.xmlsoap.org/soap/envelope/' s:encodingStyle='http://schemas.xmlsoap.org/soap/encoding/'><s:Body><u:{} xmlns:u='{}'>{}</u:{}></s:Body></s:Envelope>",
        action, AV_SERVICE, args, action
    );
    let resp = ureq::AgentBuilder::new()
        .timeout(std::time::Duration::from_secs(6))
        .build()
        .post(ctrl_url)
        .set("Content-Type", "text/xml; charset=utf-8")
        .set("SOAPACTION", &format!("\"{}#{}\"", AV_SERVICE, action))
        .send_string(&body);
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(_, _)) => Ok(()), // device answered; a SOAP fault is tolerated
        Err(e) => Err(e.to_string()),
    }
}

/// Push a local file URL to the speaker.
pub fn dlna_push(ctrl_url: &str, file_url: &str, title: &str, mime: &str) -> Result<(), String> {
    let didl = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\"><item id=\"fake_id_37\" parentID=\"fake_parentID\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>object.item.audioItem</upnp:class><res protocolInfo=\"http-get:*:{}:*\">{}</res></item></DIDL-Lite>",
        xml_escape(title), mime, xml_escape(file_url)
    );
    // The whole DIDL document is XML-escaped inside CurrentURIMetaData.
    let args = format!(
        "<InstanceID>0</InstanceID><CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
        xml_escape(file_url),
        xml_escape(&didl)
    );
    soap(ctrl_url, "SetAVTransportURI", &args)?;
    soap(ctrl_url, "Play", "<InstanceID>0</InstanceID><Speed>1</Speed>")
}