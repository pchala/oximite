//! Integration test against a localhost mock Port 8080 firmware server.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use oximite_control::client::start_client_service;
use oximite_control::store::ControlStore;
use oximite_proto::model::{DEFAULT_SETTINGS, STEP_NONE};
use oximite_proto::protocol::{
    encode_device_msg, parse_api_command, DeviceHello, DeviceMsg, ParsedCommand, TelemetryFrame,
    DIAG_MAGIC, DIAG_PROTO_VER,
};

#[test]
fn client_connects_receives_hello_ticks_and_command_reply() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    let server = thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut out = [0u8; 1024];

        let hello = DeviceMsg::Hello(DeviceHello {
            magic: DIAG_MAGIC,
            proto_ver: DIAG_PROTO_VER,
            fw_version: oximite_proto::heapless::String::try_from("0.1.0").unwrap(),
            settings: DEFAULT_SETTINGS.to_public(),
            profiles: oximite_proto::heapless::Vec::new(),
            state: 0,
            sbt: 92.0,
        });
        let n = encode_device_msg(&hello, &mut out).unwrap();
        sock.write_all(&out[..n]).unwrap();

        let tick = DeviceMsg::Tick(TelemetryFrame {
            seq: 1,
            ms: 100,
            t: 92.0,
            tt: 92.0,
            ett: 92.0,
            p: 0.0,
            tp: 0.0,
            fl: 0.0,
            tfl: 0.0,
            vol: 0.0,
            hp: 15.0,
            pump: 0.0,
            ac_us: 10000.0,
            sbt: 92.0,
            fc: 0,
            st: 0,
            step: STEP_NONE,
            ack: 0,
        });
        let n = encode_device_msg(&tick, &mut out).unwrap();
        sock.write_all(&out[..n]).unwrap();

        let mut reader = BufReader::new(sock.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let (req_id, parsed) = parse_api_command(line.trim()).unwrap();
        assert!(matches!(parsed, ParsedCommand::Ping));

        let pong = DeviceMsg::Pong {
            req_id: req_id.unwrap(),
            uptime_ms: 1234,
        };
        let n = encode_device_msg(&pong, &mut out).unwrap();
        sock.write_all(&out[..n]).unwrap();
        thread::sleep(Duration::from_millis(100));
    });

    let endpoint = addr.to_string();
    let store = Arc::new(Mutex::new(ControlStore::new(&endpoint)));
    let (client, _handle) = start_client_service(store.clone());
    client.connect(&endpoint);

    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if let Ok(st) = store.lock() {
            if st.hello.is_some() && !st.samples.is_empty() {
                break;
            }
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert!(store.lock().unwrap().hello.is_some());
    assert_eq!(store.lock().unwrap().samples.len(), 1);

    client.send_ping();
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if store.lock().unwrap().last_ping_rtt_ms.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }
    assert!(store.lock().unwrap().last_ping_rtt_ms.is_some());

    client.shutdown();
    server.join().unwrap();
}
