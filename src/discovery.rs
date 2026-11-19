//! SSDP discovery: M-SEARCH ssdp:all to 239.255.255.250:1900; responders are
//! later filtered down to MusicCast MediaRenderers.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const PORT: u16 = 1900;

/// Send an M-SEARCH and collect LOCATION URLs of responders for `wait` seconds.
pub fn search(wait: Duration) -> Vec<String> {
    let mut locations = Vec::new();
    let sock = match UdpSocket::bind("0.0.0.0:0") {
        Ok(s) => s,
        Err(_) => return locations,
    };
    let _ = sock.join_multicast_v4(&GROUP, &Ipv4Addr::UNSPECIFIED);
    let _ = sock.set_read_timeout(Some(Duration::from_millis(200)));
    let msg = format!(
        "M-SEARCH * HTTP/1.1\r\nHost: {}:{}\r\nMan: \"ssdp:discover\"\r\nMX: 2\r\nST: ssdp:all\r\nUser-Agent: upnpx/1.0\r\nConnection: close\r\n\r\n",
        GROUP, PORT
    );
    let dest = SocketAddrV4::new(GROUP, PORT);
    // Send twice for reliability (UDP may drop packets).
    for _ in 0..2 {
        let _ = sock.send_to(msg.as_bytes(), dest);
    }
    let start = Instant::now();
    let mut buf = [0u8; 8192];
    while start.elapsed() < wait {
        match sock.recv_from(&mut buf) {
            Ok((n, _)) => {
                let text = String::from_utf8_lossy(&buf[..n]).to_string();
                for line in text.lines() {
                    let lower = line.to_lowercase();
                    if let Some(rest) = lower.strip_prefix("location:") {
                        // preserve original case of the URL
                        let url = line[line.find(':').unwrap() + 1..].trim().to_string();
                        let _ = rest;
                        if !url.is_empty() && !locations.contains(&url) {
                            locations.push(url);
                        }
                    }
                }
            }
            Err(_) => continue,
        }
    }
    locations
}