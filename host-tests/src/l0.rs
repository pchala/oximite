//! L0 seeded-RNG parser fuzzing and state-machine invariant tests (mirroring `lite/host-tests/src/l0.rs`).

use oximite_proto::model::{
    BrewProfile, BrewProfileStep, MachineCommand, MachineState, DEFAULT_SETTINGS, MAX_PROFILES,
};
use oximite_proto::protocol::{
    encode_device_msg, parse_api_command, CmdError, DeviceHello, DeviceMsg, LineEvent,
    LineSplitter, RxFramer, TelemetryFrame, DIAG_MAGIC, DIAG_PROTO_VER,
};

use crate::dhcp_codec::build_reply;
use crate::http_codec::{check_request_complete, parse_http_request};
use crate::machine::Machine;

struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() >> 11) as usize % n
    }

    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
}

fn damage(rng: &mut Rng, sample: &[u8]) -> Vec<u8> {
    let mut v = sample.to_vec();
    for _ in 0..1 + rng.below(4) {
        match rng.below(4) {
            0 if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] ^= 1 << rng.below(8);
            }
            1 if !v.is_empty() => v.truncate(rng.below(v.len())),
            2 => {
                let n = 1 + rng.below(8);
                v.extend(rng.bytes(n));
            }
            _ if !v.is_empty() => {
                let i = rng.below(v.len());
                v[i] = rng.next() as u8;
            }
            _ => {}
        }
    }
    v
}

#[test]
fn http_parser_survives_random_and_damaged_requests() {
    let samples: [&[u8]; 3] = [
        b"GET /api/telemetry HTTP/1.1\r\nHost: x\r\n\r\n",
        b"GET /api/profile/4 HTTP/1.1\r\n\r\n",
        b"POST /api/cmd HTTP/1.1\r\nContent-Length: 14\r\n\r\n{\"cmd\":\"stop\"}",
    ];
    for seed in 0..2000 {
        let mut rng = Rng::new(seed);
        let sample = samples[rng.below(samples.len())];
        let mutated = damage(&mut rng, sample);
        let _ = check_request_complete(&mutated, 4096);
        if let Ok(s) = std::str::from_utf8(&mutated) {
            let _ = parse_http_request(s);
        }
    }
    for seed in 0..1000 {
        let mut rng = Rng::new(seed);
        let rand_len = rng.below(600);
        let raw = rng.bytes(rand_len);
        let _ = check_request_complete(&raw, 4096);
        if let Ok(s) = std::str::from_utf8(&raw) {
            let _ = parse_http_request(s);
        }
    }
}

#[test]
fn rx_framer_survives_random_bytes_and_chunked_streams() {
    for seed in 0..1000 {
        let mut rng = Rng::new(seed);
        let n = rng.below(2048);
        let raw = rng.bytes(n);
        let mut framer = RxFramer::<2048>::new();
        let mut at = 0;
        while at < raw.len() {
            let take = (1 + rng.below(64)).min(raw.len() - at);
            let pushed = framer.accept(&raw[at..at + take]);
            if pushed == 0 {
                framer.clear();
            } else {
                at += pushed;
            }
            while matches!(framer.next_frame(), Ok(Some(_))) {}
        }
    }

    // Valid frames split at arbitrary byte offsets must always decode intact
    let hello = DeviceMsg::Hello(DeviceHello {
        magic: DIAG_MAGIC,
        proto_ver: DIAG_PROTO_VER,
        fw_version: heapless::String::try_from("0.1.0").unwrap(),
        settings: DEFAULT_SETTINGS.to_public(),
        profiles: heapless::Vec::new(),
        state: 0,
        sbt: 92.0,
    });
    let tick = DeviceMsg::Tick(TelemetryFrame {
        seq: 100,
        ms: 2000,
        t: 93.1,
        tt: 92.0,
        ett: 92.5,
        p: 9.0,
        tp: 9.0,
        fl: 2.2,
        tfl: 2.5,
        vol: 12.0,
        hp: 30.0,
        pump: 55.0,
        ac_us: 10000.0,
        sbt: 92.0,
        fc: 0,
        st: 1,
        step: 0,
        ack: 5,
    });
    let mut wire = [0u8; 2048];
    let n1 = encode_device_msg(&hello, &mut wire).unwrap();
    let n2 = encode_device_msg(&tick, &mut wire[n1..]).unwrap();
    let stream = &wire[..n1 + n2];

    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let mut framer = RxFramer::<2048>::new();
        let mut out = Vec::new();
        let mut at = 0;
        while at < stream.len() {
            let chunk = (1 + rng.below(16)).min(stream.len() - at);
            at += framer.accept(&stream[at..at + chunk]);
            while let Some(msg) = framer.next_frame().unwrap() {
                out.push(msg);
            }
        }
        assert_eq!(out, vec![hello.clone(), tick.clone()]);
    }
}

#[test]
fn line_splitter_and_api_command_parser_survive_fuzzing() {
    let samples: [&[u8]; 4] = [
        br#"{"cmd":"stop","req_id":1}"#,
        br#"{"cmd":"set_session_temp","temp":93.5}"#,
        br#"{"cmd":"run_slot","slot":0}"#,
        br#"{"cmd":"set_stream","enabled":false,"divider":5}"#,
    ];
    for seed in 0..3000 {
        let mut rng = Rng::new(seed);
        let sample = samples[rng.below(samples.len())];
        let mut mutated = damage(&mut rng, sample);
        mutated.push(b'\n');

        let mut sp = LineSplitter::<1024>::new();
        for &b in &mutated {
            if let Some(LineEvent::Line(line)) = sp.push_byte(b) {
                if let Ok(s) = std::str::from_utf8(line) {
                    let _ = parse_api_command(s);
                }
            }
        }
    }
    assert_eq!(parse_api_command("{bad").unwrap_err(), CmdError::BadJson);
}

#[test]
fn dhcp_parser_survives_random_bytes() {
    let mut reply = [0u8; 300];
    for seed in 0..1000 {
        let mut rng = Rng::new(seed);
        let n = rng.below(400);
        let pkt = rng.bytes(n);
        let _ = build_reply(&pkt, &mut reply);
    }
}

#[test]
fn machine_state_invariants_hold_under_random_commands() {
    let empty: [Option<BrewProfile>; MAX_PROFILES as usize] =
        [const { None }; MAX_PROFILES as usize];

    for seed in 0..200 {
        let mut rng = Rng::new(seed);
        let mut m = Machine::new(DEFAULT_SETTINGS, empty.clone(), 0);
        let mut now_ms: u64 = 0;

        for step in 0..200 {
            now_ms += 50 + rng.below(500) as u64;
            let cmd = match rng.below(10) {
                0 => MachineCommand::Brew,
                1 => MachineCommand::Stop,
                2 => MachineCommand::Steam,
                3 => MachineCommand::Flush,
                4 => MachineCommand::TogglePower,
                5 => MachineCommand::DirectPump(65.0),
                6 => MachineCommand::SetSessionTemp(90.0 + (step % 6) as f32),
                7 => {
                    let mut ms = DEFAULT_SETTINGS.machine;
                    ms.brew_temp = 91.0 + (step % 4) as f32;
                    MachineCommand::SaveMachine(ms)
                }
                8 => {
                    let mut p = BrewProfile {
                        name: heapless::String::try_from("Fuzz").unwrap(),
                        steps: heapless::Vec::new(),
                    };
                    let _ = p.steps.push(BrewProfileStep {
                        time_s: Some(5.0),
                        volume: None,
                        pressure: Some(8.0),
                        flow: None,
                    });
                    MachineCommand::SaveProfile((step % 10) as u8, p)
                }
                _ => MachineCommand::DeleteProfile((step % 10) as u8),
            };

            let eff = m.handle_command(cmd, now_ms);
            // Invariant 1: No flash writes may be triggered while busy
            if m.state().is_busy() {
                assert_eq!(eff.dirty_flash, 0, "seed {seed}: dirty flash while busy");
            }
            // Invariant 2: Once non-busy, all deferred writes must have flushed
            if !m.state().is_busy() {
                assert!(!m.has_deferred_writes(), "seed {seed}: unflushed writes");
            }
            // Invariant 3: Valve is never open when idle or sleeping
            if matches!(m.state(), MachineState::Idle | MachineState::Sleeping) {
                assert!(!m.valve_open());
            }

            let _ = m.tick(now_ms + 20, (step % 40) as f32, 92.0);
        }

        // Stop always recovers to non-busy state with closed valve and flushed writes
        m.handle_command(MachineCommand::Stop, now_ms + 50);
        assert!(!m.state().is_busy());
        assert!(!m.valve_open());
        assert!(!m.has_deferred_writes());
    }
}

#[test]
fn proto_models_and_protocol_helpers_roundtrip_all_variants() {
    use oximite_proto::model::{default_profile, Settings, Telemetry, WifiSettings};
    use oximite_proto::protocol::{
        round2, ApiCommand, FrameError, ParsedCommand, UiTelemetry, MAX_FRAME_LEN,
    };

    // MachineState conversions and labels
    for raw in 0..=6u8 {
        let st = MachineState::try_from(raw).unwrap();
        assert_eq!(u8::from(st), raw);
        assert!(!st.as_str().is_empty());
    }
    assert_eq!(MachineState::try_from(7), Err(7));

    // Settings default and MachineCommand names
    let def_s = Settings::default();
    assert_eq!(def_s, DEFAULT_SETTINGS);
    let wifi = WifiSettings {
        ssid: heapless::String::try_from("Net").unwrap(),
        password: heapless::String::try_from("Pass1234").unwrap(),
    };
    let cmds = [
        MachineCommand::RunProfile(default_profile()),
        MachineCommand::Brew,
        MachineCommand::Stop,
        MachineCommand::Steam,
        MachineCommand::Flush,
        MachineCommand::DirectPump(50.0),
        MachineCommand::SaveMachine(def_s.machine),
        MachineCommand::SavePids(def_s.temp_pid, def_s.pump_pid),
        MachineCommand::SaveWifi(wifi.clone()),
        MachineCommand::SaveProfile(0, default_profile()),
        MachineCommand::DeleteProfile(0),
        MachineCommand::TogglePower,
        MachineCommand::SetSessionTemp(93.0),
    ];
    for c in &cmds {
        assert!(!c.name().is_empty());
    }

    // Telemetry display_temp and UiTelemetry rounding
    let snap = Telemetry {
        temp_c: 102.346,
        pressure_bar: 8.956,
        flow_rate_ml_s: 2.104,
        volume_ml: 36.005,
        ..Default::default()
    };
    assert!((snap.display_temp(10.0, false) - 92.346).abs() < 1e-3);
    assert!((snap.display_temp(10.0, true) - 102.346).abs() < 1e-3);
    assert_eq!(round2(-1.236), -1.24);
    let ui_brew = UiTelemetry::from_snapshot(&snap, 10.0, false, 92.0, 1, 11);
    assert_eq!(ui_brew.t, 92.35);
    assert_eq!(ui_brew.ack, 11);
    let ui_steam = UiTelemetry::from_snapshot(&snap, 10.0, true, 92.0, 2, 12);
    assert_eq!(ui_steam.t, 102.35);

    // CmdError labels and ApiCommand builders
    for err in [
        CmdError::BadJson,
        CmdError::UnknownCommand,
        CmdError::MissingField,
        CmdError::NotFound,
        CmdError::Busy,
        CmdError::InvalidSlot,
    ] {
        assert!(!err.as_str().is_empty());
    }
    let built = ApiCommand::simple("ping").with_req_id(77);
    assert_eq!(built.cmd, "ping");
    assert_eq!(built.req_id, Some(77));

    // Exercise all parse_api_command branches and slot bounds
    let cases = [
        r#"{"cmd":"power"}"#,
        r#"{"cmd":"brew"}"#,
        r#"{"cmd":"stop"}"#,
        r#"{"cmd":"steam"}"#,
        r#"{"cmd":"flush"}"#,
        r#"{"cmd":"direct_pump","power":60.0}"#,
        r#"{"cmd":"set_session_temp","temp":94.0}"#,
        r#"{"cmd":"save_machine","machine":{"brew_temp":92.0,"steam_temp":135.0,"steam_time_limit_s":120.0,"sleep_timeout_min":20.0,"temp_offset":10.0,"flow_pulses_per_liter":98324.0,"feed_forward_percents":100.0}}"#,
        r#"{"cmd":"save_pids","temp_pid":{"kp":8.0,"ki":0.8,"kd":20.0},"pump_pid":{"kp":12.0,"ki":36.0,"kd":0.0}}"#,
        r#"{"cmd":"save_wifi","wifi":{"ssid":"A","password":"B"}}"#,
        r#"{"cmd":"profile","profile":{"name":"P","steps":[]}}"#,
        r#"{"cmd":"run_slot","slot":0}"#,
        r#"{"cmd":"save_profile","slot":1,"profile":{"name":"P","steps":[]}}"#,
        r#"{"cmd":"delete_profile","slot":1}"#,
        r#"{"cmd":"ping","req_id":5}"#,
        r#"{"cmd":"get_settings"}"#,
        r#"{"cmd":"get_profiles"}"#,
        r#"{"cmd":"get_profile","slot":0}"#,
        r#"{"cmd":"set_stream","enabled":true,"divider":2}"#,
    ];
    for json in cases {
        assert!(parse_api_command(json).is_ok(), "failed to parse {json}");
    }
    for bad_slot in [
        r#"{"cmd":"run_slot","slot":10}"#,
        r#"{"cmd":"save_profile","slot":10,"profile":{"name":"P","steps":[]}}"#,
        r#"{"cmd":"delete_profile","slot":10}"#,
        r#"{"cmd":"get_profile","slot":10}"#,
    ] {
        assert_eq!(parse_api_command(bad_slot), Err(CmdError::InvalidSlot));
    }
    let (_, stream_def) = parse_api_command(r#"{"cmd":"set_stream"}"#).unwrap();
    assert_eq!(
        stream_def,
        ParsedCommand::SetStream {
            enabled: true,
            divider: 1
        }
    );

    // Frame encoder edge cases, RxFramer errors, and LineSplitter overflow
    let pong = DeviceMsg::Pong {
        req_id: 1,
        uptime_ms: 500,
    };
    let mut tiny = [0u8; 1];
    assert_eq!(encode_device_msg(&pong, &mut tiny), None);
    let mut small = [0u8; 3];
    assert_eq!(encode_device_msg(&pong, &mut small), None);

    let mut framer = RxFramer::<64>::default();
    assert_eq!(framer.buffered(), 0);
    framer.accept(&[0, 0]);
    assert_eq!(framer.next_frame(), Err(FrameError::TooLarge(0)));
    let over = ((MAX_FRAME_LEN + 1) as u16).to_le_bytes();
    framer.accept(&over);
    assert_eq!(
        framer.next_frame(),
        Err(FrameError::TooLarge(MAX_FRAME_LEN + 1))
    );
    framer.accept(&[2, 0, 0xFF, 0xFF]);
    assert_eq!(framer.next_frame(), Err(FrameError::Decode));
    framer.accept(&[1, 2, 3]);
    assert_eq!(framer.buffered(), 3);
    framer.clear();
    assert_eq!(framer.buffered(), 0);

    let mut sp = LineSplitter::<8>::default();
    for &b in b"0123456789\nok\r\n" {
        let _ = sp.push_byte(b);
    }
}
