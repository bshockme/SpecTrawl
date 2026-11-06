//! Just enough HTTP/1.1 for a loopback API that answers with a `Content-Length` (OnAir's remote API always does):
//! one GET per connection.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

const TIMEOUT: Duration = Duration::from_secs(2);
/// Answers larger than this are refused.
const MAX_BODY: usize = 8 << 20;

/// `GET path` from `127.0.0.1:port`, its body as JSON. Errors name what went wrong.
pub fn get_json(port: u16, path: &str) -> Result<Value, String> {
    let body = get(port, path)?;
    serde_json::from_slice(&body).map_err(|e| format!("{path}: not JSON: {e}"))
}

/// `GET path`, the body of a 200 answer.
pub fn get(port: u16, path: &str) -> Result<Vec<u8>, String> {
    let mut s = TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), TIMEOUT).map_err(|e| e.to_string())?;
    s.set_read_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    s.set_write_timeout(Some(TIMEOUT)).map_err(|e| e.to_string())?;
    write!(s, "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n").map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 16384];
    let (head_end, length) = loop {
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            return Err(format!("{path}: closed before the headers"));
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&buf[..i]).to_string();
            let status = head.split_whitespace().nth(1).unwrap_or_default().to_string();
            if status != "200" {
                return Err(format!("{path}: HTTP {status}"));
            }
            let length = head
                .lines()
                .find_map(|l| {
                    l.split_once(':').filter(|(k, _)| k.eq_ignore_ascii_case("content-length")).map(|(_, v)| v.trim().to_string())
                })
                .and_then(|v| v.parse::<usize>().ok());
            break (i + 4, length);
        }
        if buf.len() > 64 << 10 {
            return Err(format!("{path}: headers too long"));
        }
    };
    let want = length.unwrap_or(usize::MAX).min(MAX_BODY);
    while buf.len() - head_end < want {
        let n = s.read(&mut chunk).map_err(|e| e.to_string())?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    if let Some(l) = length
        && buf.len() - head_end < l
    {
        return Err(format!("{path}: {} of {l} bytes", buf.len() - head_end));
    }
    Ok(buf.split_off(head_end))
}
