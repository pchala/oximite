//! Integration test against localhost mock Port 8080 (telemetry) and Port 80 (HTTP) servers.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oximite_control::client::start_client_service;
use oximite_control::store::ControlStore;
use oximite_proto::model::{DEFAULT_SETTINGS, STEP_NONE};
use oximite_proto::protocol::{encode_device_msg, DeviceMsg, WireTick};

fn read_http_request(sock: &mut TcpStream) -> (String, String) {
    let mut raw = Vec::new();
    let mut buf = [0u8; 512];
    loop {
        let n = sock.read(&mut buf).unwrap();
        raw.extend_from_slice(&buf[..n]);
        let text = String::from_utf8_lossy(&raw).to_string();
        if let Some((head, body)) = text.split_once("\r\n\r\n") {
            let want = head
                .lines()
                .find_map(|l| l.strip_prefix("Content-Length: "))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if body.len() >= want {
                let first = head.lines().next().unwrap().to_string();
                return (first, body.to_string());
            }
        }
        assert!(n > 0, "peer closed early");
    }
}

fn mock_http(listener: TcpListener, commands: Arc<Mutex<Vec<String>>>, stop: Arc<AtomicBool>) {
    listener.set_nonblocking(true).unwrap();
    let settings = serde_json::to_string(&DEFAULT_SETTINGS.to_public()).unwrap();
    while !stop.load(Ordering::Relaxed) {
        let Ok((mut sock, _)) = listener.accept() else {
            thread::sleep(Duration::from_millis(5));
            continue;
        };
        sock.set_nonblocking(false).unwrap();
        let (line, body) = read_http_request(&mut sock);
        let reply = match line.as_str() {
            l if l.starts_with("GET /api/settings") => settings.clone(),
            l if l.starts_with("GET /api/profiles") => "[]".to_string(),
            l if l.starts_with("GET /api/telemetry") => "{}".to_string(),
            l if l.starts_with("POST /api/cmd") => {
                commands.lock().unwrap().push(body);
                r#"{"status":"ok","ack":1}"#.to_string()
            }
            _ => String::new(),
        };
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        );
        sock.write_all(resp.as_bytes()).unwrap();
    }
}

fn wait_for(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        thread::sleep(Duration::from_millis(10));
    }
    panic!("timed out waiting for {what}");
}

#[test]
fn client_streams_telemetry_and_uses_http_for_commands() {
    let tele = TcpListener::bind("127.0.0.1:0").unwrap();
    let http = TcpListener::bind("127.0.0.1:0").unwrap();
    let tele_addr = tele.local_addr().unwrap();
    let http_port = http.local_addr().unwrap().port();

    let commands = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let http_thread = {
        let (c, s) = (commands.clone(), stop.clone());
        thread::spawn(move || mock_http(http, c, s))
    };

    let tele_thread = thread::spawn(move || {
        let (mut sock, _) = tele.accept().unwrap();
        let mut out = [0u8; 1024];
        let tick = DeviceMsg::Tick(WireTick {
            seq: 1,
            dt_us: 10_000,
            t: 9200,
            tt: 9200,
            p: 0,
            tp: 0,
            fl: 0,
            tfl: 0,
            vol: 0,
            hp: 15,
            pump: 0,
            sbt: 9200,
            flags: 0,
            step: STEP_NONE,
        });
        let n = encode_device_msg(&tick, &mut out).unwrap();
        sock.write_all(&out[..n]).unwrap();
        thread::sleep(Duration::from_millis(1500));
    });

    let endpoint = tele_addr.to_string();
    let store = Arc::new(Mutex::new(ControlStore::new(&endpoint)));
    let (client, _handle) = start_client_service(store.clone());
    client.set_http_port(http_port);
    client.connect(&endpoint);

    wait_for("tick", || !store.lock().unwrap().samples.is_empty());
    wait_for("settings over HTTP", || {
        store.lock().unwrap().settings.is_some()
    });

    client.send_ping();
    wait_for("HTTP rtt", || {
        store.lock().unwrap().last_ping_rtt_ms.is_some()
    });

    client.send_simple("stop");
    wait_for("command POST", || !commands.lock().unwrap().is_empty());
    assert!(commands.lock().unwrap()[0].contains("\"stop\""));

    client.shutdown();
    stop.store(true, Ordering::Relaxed);
    tele_thread.join().unwrap();
    http_thread.join().unwrap();
}
