//! A stand-in for OnAir's `onair-server`, for tests: it reads the same settings file, connects to the rtl_tcp feed
//! `net_radio` names and answers `/api/v1/status`, `/api/v1/telemetry` and `/api/v1/events` the way OnAir 3.6.0 does,
//! with one decoded message once enough samples have arrived.
//!
//! Test switches in the settings: `fake_device=<name>` claims that radio instead of the feed; `fake_exit_after_secs=N`
//! exits after N seconds on its first run (a marker in `$HOME` makes a restart run normally); `fake_event_after_bytes=N`.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let conf = args.iter().position(|a| a == "--config").and_then(|i| args.get(i + 1)).expect("--config FILE");
    let text = std::fs::read_to_string(conf).expect("the settings file");
    let kv: HashMap<String, String> = text
        .lines()
        .filter(|l| !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let port: u16 = kv["port"].parse().expect("port");
    let net = kv.get("net_radio").cloned().unwrap_or_default();
    let feed: Vec<&str> = net.split(',').collect();
    let freq_hz = kv.get("freq_mhz").and_then(|f| f.parse::<f64>().ok()).unwrap_or(0.0) * 1e6;
    let mode = kv.get("mode").cloned().unwrap_or_default();
    let event_after: u64 = kv.get("fake_event_after_bytes").and_then(|v| v.parse().ok()).unwrap_or(500_000);
    let device = match (kv.get("fake_device"), kv.get("device").map(String::as_str)) {
        (Some(d), _) => d.clone(),
        (None, Some("rtl_tcp")) => format!("rtl_tcp {}:{} (network)", feed.get(1).unwrap_or(&"?"), feed.get(2).unwrap_or(&"?")),
        _ => "Airspy 2F650AE3 (native, experimental)".into(),
    };
    let home = std::env::var("HOME").unwrap_or_default();
    let marker = std::path::Path::new(&home).join("fake-ran-once");
    let exit_after = kv.get("fake_exit_after_secs").and_then(|v| v.parse::<f64>().ok()).filter(|_| !marker.exists());
    let _ = std::fs::write(&marker, b"");

    let bytes = Arc::new(AtomicU64::new(0));
    if device.starts_with("rtl_tcp") && feed.len() == 3 {
        let (b, addr) = (bytes.clone(), format!("{}:{}", feed[1], feed[2]));
        std::thread::spawn(move || {
            let Ok(mut s) = TcpStream::connect(addr) else { return };
            let mut greeting = [0u8; 12];
            if s.read_exact(&mut greeting).is_err() {
                return;
            }
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = s.read(&mut buf) {
                if n == 0 {
                    break;
                }
                b.fetch_add(n as u64, Ordering::Relaxed);
            }
        });
    }
    let listener = TcpListener::bind(("127.0.0.1", port)).expect("the control port");
    listener.set_nonblocking(true).unwrap();
    let t0 = Instant::now();
    let mut event_time: Option<f64> = None;
    loop {
        if exit_after.is_some_and(|s| t0.elapsed().as_secs_f64() > s) {
            eprintln!("fake: exiting as asked");
            std::process::exit(3);
        }
        let n = bytes.load(Ordering::Relaxed);
        if n >= event_after && event_time.is_none() {
            event_time = Some(SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs_f64());
        }
        match listener.accept() {
            Ok((s, _)) => {
                let _ = answer(s, &device, &mode, freq_hz, n, event_time);
            }
            Err(_) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn answer(mut s: TcpStream, device: &str, mode: &str, freq_hz: f64, bytes: u64, event_time: Option<f64>) -> std::io::Result<()> {
    s.set_nonblocking(false)?;
    let mut line = String::new();
    BufReader::new(&s).read_line(&mut line)?;
    let path = line.split_whitespace().nth(1).unwrap_or("/");
    let (route, query) = path.split_once('?').unwrap_or((path, ""));
    let since = query.split('&').find_map(|kv| kv.strip_prefix("since=")).and_then(|v| v.parse::<u64>().ok());
    let body = match route {
        "/api/v1/status" => {
            format!(r#"{{"running":true,"device":"{device}","deviceKind":"native","modeId":"{mode}","frequencyHz":{freq_hz}}}"#)
        }
        "/api/v1/telemetry" => {
            let locked = bytes > 0;
            format!(
                r#"{{"running":true,"state":{},"locked":{locked},"blocksOk":{},"blocksBad":0}}"#,
                if locked { 2 } else { 0 },
                bytes / 4096
            )
        }
        "/api/v1/events" => {
            // One message, number 1; the cursor is past it once it exists.
            let next = u64::from(event_time.is_some());
            let events = match (event_time, since) {
                (Some(t), Some(0)) => format!(
                    r#"[{{"v":1,"type":"pager.message","time":{t},"mode":"FLEX","freqHz":{freq_hz},"fields":{{"text":"fake page","capcode":1234}}}}]"#
                ),
                _ => String::new(),
            };
            format!(r#"{{"gap":false,"next":{next},"events":[{}]}}"#, events.trim_start_matches('[').trim_end_matches(']'))
        }
        _ => {
            write!(s, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")?;
            return Ok(());
        }
    };
    write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
}
