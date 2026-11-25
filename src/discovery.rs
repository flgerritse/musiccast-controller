//! SSDP discovery: M-SEARCH to 239.255.255.250:1900; responders are later
//! filtered down to MusicCast MediaRenderers.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;

/// Send M-SEARCHes and collect unique LOCATION URLs of responders for `wait`.
/// Only one location per host is kept: MusicCast devices answer for several
/// UPnP services, but they share one description.
pub fn search(wait: Duration) -> Vec<String> {
    let mut locations: Vec<String> = Vec::new();
    let Ok(sock) = UdpSocket::bind("0.0.0.0:0") else { return locations };
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let dest = SocketAddrV4::new(GROUP, PORT);
    // MediaRenderer targets the speakers directly; ssdp:all catches firmware
    // that only answers the generic search. Each is sent twice (UDP may drop).
    for st in ["urn:schemas-upnp-org:device:MediaRenderer:1", "ssdp:all"] {
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {GROUP}:{PORT}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        );
        for _ in 0..2 {
            let _ = sock.send_to(msg.as_bytes(), dest);
        }
    }
    let start = Instant::now();
    let mut buf = [0u8; 8192];
    while start.elapsed() < wait {
        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
        let text = String::from_utf8_lossy(&buf[..n]);
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else { continue };
            if !key.trim().eq_ignore_ascii_case("location") {
                continue;
            }
            let url = value.trim().to_string();
            let host = crate::api::host_of(&url);
            let renderer = url.to_lowercase().contains("mediarenderer");
            match locations.iter().position(|l| crate::api::host_of(l) == host) {
                // Prefer the MediaRenderer description over other services of the same host.
                Some(pos) if renderer && !locations[pos].to_lowercase().contains("mediarenderer") => locations[pos] = url,
                Some(_) => {}
                None if !url.is_empty() => locations.push(url),
                None => {}
            }
        }
    }
    locations
}
