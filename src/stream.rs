//! Local music streaming: a small HTTP file server (port 9999, +10 retry up to
//! 100 times) plus UPnP AVTransport push (SetAVTransportURI + Play, DIDL-Lite
//! metadata).

use crate::api;
use std::io::{Read, Seek, SeekFrom};
use std::net::UdpSocket;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server, StatusCode};

pub const SUPPORTED_EXTS: [&str; 12] =
    ["mp3", "wma", "aac", "ogg", "flac", "alac", "aiff", "aif", "wav", "mp4", "ape", "m4a"];

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
        "m4a" | "alac" | "mp4" => "audio/mp4",
        "aac" => "audio/aac",
        "ogg" => "audio/ogg",
        "aiff" | "aif" => "audio/aiff",
        "wma" => "audio/x-ms-wma",
        "ape" => "audio/x-ape",
        _ => "application/octet-stream",
    }
}

/// The local address the speaker can reach us on: "connect" a UDP socket to the
/// speaker (no traffic is sent) and read the source address the OS picked. This
/// stays correct with a VPN or several network interfaces.
pub fn local_ip_for(target_ip: &str) -> String {
    if let Ok(sock) = UdpSocket::bind("0.0.0.0:0") {
        if sock.connect((target_ip, 80)).is_ok() {
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
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
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

    /// URL `/<index>/<name>`: the index makes equal file names from different
    /// folders unambiguous; the name is only there for the renderer's display.
    pub fn url_for(&self, index: usize, path: &Path, speaker_ip: &str) -> String {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
        format!("http://{}:{}/{}/{}", local_ip_for(speaker_ip), self.port, index, url_encode_path(name))
    }
}

fn serve_loop(server: Server, files: Arc<Mutex<Vec<PathBuf>>>) {
    for request in server.incoming_requests() {
        if request.method() != &Method::Get && request.method() != &Method::Head {
            let _ = request.respond(Response::from_string("method not allowed").with_status_code(StatusCode(405)));
            continue;
        }
        let url_path = request.url().split('?').next().unwrap_or("/").to_string();
        let mut parts = url_path.trim_start_matches('/').splitn(2, '/');
        let index = parts.next().and_then(|p| p.parse::<usize>().ok());
        let name = parts.next().map(api::percent_decode).unwrap_or_default();
        let found = index.and_then(|i| files.lock().unwrap().get(i).cloned()).filter(|p| {
            p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n == name)
        });
        let Some(path) = found else {
            let _ = request.respond(Response::from_string("not found").with_status_code(StatusCode(404)));
            continue;
        };
        let Ok(mut file) = std::fs::File::open(&path) else {
            let _ = request.respond(Response::from_string("open failed").with_status_code(StatusCode(500)));
            continue;
        };
        let total = file.metadata().map(|m| m.len()).unwrap_or(0);
        let mime = mime_for(&path);
        let mut headers = vec![
            Header::from_bytes(&b"Content-Type"[..], mime.as_bytes()).unwrap(),
            Header::from_bytes(&b"Accept-Ranges"[..], &b"bytes"[..]).unwrap(),
        ];
        let range = request
            .headers()
            .iter()
            .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case("Range"))
            .map(|h| h.value.as_str().to_string());
        // Single-range support, streamed from disk (never buffered in memory).
        let (status, start, len) = match range {
            Some(r) => match parse_range(&r, total) {
                Some((start, end)) => {
                    headers.push(
                        Header::from_bytes(&b"Content-Range"[..], format!("bytes {start}-{end}/{total}").as_bytes())
                            .unwrap(),
                    );
                    (206, start, end - start + 1)
                }
                None => {
                    let cr = Header::from_bytes(&b"Content-Range"[..], format!("bytes */{total}").as_bytes()).unwrap();
                    let _ = request.respond(Response::empty(416).with_header(cr));
                    continue;
                }
            },
            None => (200, 0, total),
        };
        if file.seek(SeekFrom::Start(start)).is_err() {
            let _ = request.respond(Response::from_string("seek failed").with_status_code(StatusCode(500)));
            continue;
        }
        let resp = Response::new(StatusCode(status), headers, file.take(len), Some(len as usize), None);
        let _ = request.respond(resp);
    }
}

/// Parse a single `bytes=` range; None when it is malformed or unsatisfiable.
fn parse_range(value: &str, total: u64) -> Option<(u64, u64)> {
    let v = value.trim().strip_prefix("bytes=")?;
    let (a, b) = v.split(',').next()?.split_once('-')?;
    let (a, b) = (a.trim(), b.trim());
    if total == 0 {
        return None;
    }
    if a.is_empty() {
        // suffix range: last N bytes
        let n: u64 = b.parse().ok()?;
        if n == 0 {
            return None;
        }
        Some((total.saturating_sub(n), total - 1))
    } else {
        let start: u64 = a.parse().ok()?;
        let end: u64 = if b.is_empty() { total - 1 } else { b.parse::<u64>().ok()?.min(total - 1) };
        (start <= end).then_some((start, end))
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
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action} xmlns:u=\"{AV_SERVICE}\">{args}</u:{action}></s:Body></s:Envelope>"
    );
    let resp = api::agent()
        .post(ctrl_url)
        .timeout(std::time::Duration::from_secs(6))
        .set("Content-Type", "text/xml; charset=\"utf-8\"")
        .set("SOAPACTION", &format!("\"{AV_SERVICE}#{action}\""))
        .send_string(&body);
    match resp {
        Ok(_) => Ok(()),
        Err(ureq::Error::Status(code, r)) => {
            // Report the UPnP fault instead of pretending it worked.
            let text = r.into_string().unwrap_or_default();
            let desc = roxmltree::Document::parse(&text)
                .ok()
                .and_then(|d| d.descendants().find(|n| n.has_tag_name("errorDescription")).and_then(|n| n.text().map(String::from)));
            Err(format!("{action}: HTTP {code}{}", desc.map(|d| format!(" ({d})")).unwrap_or_default()))
        }
        Err(e) => Err(e.to_string()),
    }
}

/// Push a local file URL to the speaker.
pub fn dlna_push(ctrl_url: &str, file_url: &str, title: &str, mime: &str) -> Result<(), String> {
    let didl = format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" xmlns:dlna=\"urn:schemas-dlna-org:metadata-1-0/\"><item id=\"1\" parentID=\"0\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>object.item.audioItem.musicTrack</upnp:class><res protocolInfo=\"http-get:*:{}:*\">{}</res></item></DIDL-Lite>",
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        assert_eq!(parse_range("bytes=0-", 1000), Some((0, 999)));
        assert_eq!(parse_range("bytes=100-199", 1000), Some((100, 199)));
        assert_eq!(parse_range("bytes=-100", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=900-5000", 1000), Some((900, 999)));
        assert_eq!(parse_range("bytes=500-100", 1000), None);
        assert_eq!(parse_range("bytes=1000-", 1000), None);
        assert_eq!(parse_range("bytes=-0", 1000), None);
        assert_eq!(parse_range("bytes=x-1", 1000), None);
    }
}
