//! Wire protocol codecs shared by HTTP (Port 80) and the TCP stream (Port 8080).
//!
//! - Uplink (both Port 80 `POST /api/cmd` and Port 8080 newline-delimited JSON)
//!   shares [`ApiCommand`] and [`parse_api_command`].
//! - Downlink on Port 8080 uses length-prefixed Postcard v3 frames
//!   `[len: u16 LE][postcard DeviceMsg]`, encoded with [`encode_device_msg`] and
//!   decoded incrementally with [`RxFramer`].

use serde::{Deserialize, Serialize};

use crate::model::{
    BrewProfile, MachineCommand, MachineSettings, PidSettings, ProfileHeaderOwned, PublicSettings,
    Telemetry, WifiSettings, MAX_PROFILES,
};

pub const DIAG_MAGIC: u32 = 0x4D49_584F; // "OXIM" little-endian
pub const DIAG_PROTO_VER: u8 = 3;
pub const MAX_FRAME_LEN: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CmdError {
    BadJson,
    UnknownCommand,
    MissingField,
    NotFound,
    Busy,
    InvalidSlot,
}

impl CmdError {
    pub fn as_str(self) -> &'static str {
        match self {
            CmdError::BadJson => "bad_json",
            CmdError::UnknownCommand => "unknown_command",
            CmdError::MissingField => "missing_field",
            CmdError::NotFound => "not_found",
            CmdError::Busy => "queue_full",
            CmdError::InvalidSlot => "invalid_slot",
        }
    }
}

/// JSON command schema shared by Port 80 `POST /api/cmd` and Port 8080 JSON uplink.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApiCommand<'a> {
    pub cmd: &'a str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub req_id: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<BrewProfile>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<MachineSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp_pid: Option<PidSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pump_pid: Option<PidSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wifi: Option<WifiSettings>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temp: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub divider: Option<u8>,
}

impl<'a> ApiCommand<'a> {
    pub fn simple(cmd: &'a str) -> Self {
        Self {
            cmd,
            ..Default::default()
        }
    }

    pub fn with_req_id(mut self, req_id: u32) -> Self {
        self.req_id = Some(req_id);
        self
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ParsedCommand {
    Machine(MachineCommand),
    RunSlot(u8),
    Ping,
    GetSettings,
    GetProfiles,
    GetProfile(u8),
    SetStream { enabled: bool, divider: u8 },
}

fn req<T>(v: Option<T>) -> Result<T, CmdError> {
    v.ok_or(CmdError::MissingField)
}

pub fn parse_api_command_struct(
    payload: ApiCommand<'_>,
) -> Result<(Option<u32>, ParsedCommand), CmdError> {
    let req_id = payload.req_id;
    let parsed = match payload.cmd {
        "power" => ParsedCommand::Machine(MachineCommand::TogglePower),
        "brew" => ParsedCommand::Machine(MachineCommand::Brew),
        "stop" => ParsedCommand::Machine(MachineCommand::Stop),
        "steam" => ParsedCommand::Machine(MachineCommand::Steam),
        "flush" => ParsedCommand::Machine(MachineCommand::Flush),
        "direct_pump" => ParsedCommand::Machine(MachineCommand::DirectPump(req(payload.power)?)),
        "set_session_temp" => {
            ParsedCommand::Machine(MachineCommand::SetSessionTemp(req(payload.temp)?))
        }
        "save_machine" => {
            ParsedCommand::Machine(MachineCommand::SaveMachine(req(payload.machine)?))
        }
        "save_pids" => ParsedCommand::Machine(MachineCommand::SavePids(
            req(payload.temp_pid)?,
            req(payload.pump_pid)?,
        )),
        "save_wifi" => ParsedCommand::Machine(MachineCommand::SaveWifi(req(payload.wifi)?)),
        "profile" => ParsedCommand::Machine(MachineCommand::RunProfile(req(payload.profile)?)),
        "run_slot" => {
            let slot = req(payload.slot)?;
            if slot >= MAX_PROFILES {
                return Err(CmdError::InvalidSlot);
            }
            ParsedCommand::RunSlot(slot)
        }
        "save_profile" => {
            let slot = req(payload.slot)?;
            if slot >= MAX_PROFILES {
                return Err(CmdError::InvalidSlot);
            }
            ParsedCommand::Machine(MachineCommand::SaveProfile(slot, req(payload.profile)?))
        }
        "delete_profile" => {
            let slot = req(payload.slot)?;
            if slot >= MAX_PROFILES {
                return Err(CmdError::InvalidSlot);
            }
            ParsedCommand::Machine(MachineCommand::DeleteProfile(slot))
        }
        "ping" => ParsedCommand::Ping,
        "get_settings" => ParsedCommand::GetSettings,
        "get_profiles" => ParsedCommand::GetProfiles,
        "get_profile" => {
            let slot = req(payload.slot)?;
            if slot >= MAX_PROFILES {
                return Err(CmdError::InvalidSlot);
            }
            ParsedCommand::GetProfile(slot)
        }
        "set_stream" => ParsedCommand::SetStream {
            enabled: payload.enabled.unwrap_or(true),
            divider: payload.divider.unwrap_or(1).max(1),
        },
        _ => return Err(CmdError::UnknownCommand),
    };
    Ok((req_id, parsed))
}

pub fn parse_api_command(json: &str) -> Result<(Option<u32>, ParsedCommand), CmdError> {
    let (payload, _) =
        serde_json_core::from_str::<ApiCommand>(json).map_err(|_| CmdError::BadJson)?;
    parse_api_command_struct(payload)
}

/// Compact JSON snapshot served by `GET /api/telemetry`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct UiTelemetry {
    pub t: f32,
    pub sbt: f32,
    pub p: f32,
    pub fl: f32,
    pub vol: f32,
    pub st: u32,
    pub ack: u32,
}

/// Rounds a float to 2 decimal places without `std`.
pub fn round2(v: f32) -> f32 {
    let bias = if v < 0.0 { -0.5 } else { 0.5 };
    ((v * 100.0 + bias) as i32) as f32 / 100.0
}

impl UiTelemetry {
    pub fn from_snapshot(
        snap: &Telemetry,
        temp_offset: f32,
        is_steaming: bool,
        session_brew_temp: f32,
        state: u8,
        ack: u32,
    ) -> Self {
        let disp_t = snap.display_temp(temp_offset, is_steaming);
        Self {
            t: round2(disp_t),
            sbt: round2(session_brew_temp),
            p: round2(snap.pressure_bar),
            fl: round2(snap.flow_rate_ml_s),
            vol: round2(snap.volume_ml),
            st: state as u32,
            ack,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommandAck {
    pub status: &'static str,
    pub ack: u32,
}

/// 50 Hz telemetry sample sent inside `DeviceMsg::Tick`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryFrame {
    pub seq: u32,
    pub ms: u32,
    pub t: f32,
    pub tt: f32,
    pub ett: f32,
    pub p: f32,
    pub tp: f32,
    pub fl: f32,
    pub tfl: f32,
    pub vol: f32,
    pub hp: f32,
    pub pump: f32,
    pub ac_us: f32,
    pub sbt: f32,
    pub fc: u8,
    pub st: u8,
    pub step: u8,
    pub ack: u32,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct DeviceHello {
    pub magic: u32,
    pub proto_ver: u8,
    pub fw_version: heapless::String<16>,
    pub settings: PublicSettings,
    pub profiles: heapless::Vec<ProfileHeaderOwned, { MAX_PROFILES as usize }>,
    pub state: u8,
    pub sbt: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepReason {
    Started,
    TimeLimit,
    VolumeLimit,
    SafetyTimeout,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeviceEvent {
    StateChanged { from: u8, to: u8 },
    StepChanged { index: u8, reason: StepReason },
    SettingsUpdated,
    ProfilesUpdated,
    FlashSaved,
}

/// Typed Port 8080 downlink message envelope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum DeviceMsg {
    Hello(DeviceHello),
    Tick(TelemetryFrame),
    CommandReply {
        req_id: u32,
        ticket: Option<u32>,
        err: Option<CmdError>,
    },
    SettingsSnapshot(PublicSettings),
    ProfilesList(heapless::Vec<ProfileHeaderOwned, { MAX_PROFILES as usize }>),
    ProfileData {
        slot: u8,
        profile: Option<BrewProfile>,
    },
    Event(DeviceEvent),
    Pong {
        req_id: u32,
        uptime_ms: u32,
    },
}

/// Encodes `msg` as `[len: u16 LE][postcard bytes]` into `out`, returning total byte count.
pub fn encode_device_msg(msg: &DeviceMsg, out: &mut [u8]) -> Option<usize> {
    if out.len() < 2 {
        return None;
    }
    let payload_len = postcard::to_slice(msg, &mut out[2..]).ok()?.len();
    if payload_len > MAX_FRAME_LEN || payload_len > u16::MAX as usize {
        return None;
    }
    let hdr = (payload_len as u16).to_le_bytes();
    out[0] = hdr[0];
    out[1] = hdr[1];
    Some(2 + payload_len)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameError {
    TooLarge(usize),
    Decode,
}

/// Incremental TCP stream deframer for `[len: u16 LE][postcard DeviceMsg]`.
pub struct RxFramer<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Default for RxFramer<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> RxFramer<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
        }
    }

    pub fn buffered(&self) -> usize {
        self.len
    }

    /// Appends up to the remaining buffer space and returns how many bytes were copied.
    pub fn accept(&mut self, bytes: &[u8]) -> usize {
        let take = bytes.len().min(N - self.len);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        take
    }

    pub fn clear(&mut self) {
        self.len = 0;
    }

    pub fn next_frame(&mut self) -> Result<Option<DeviceMsg>, FrameError> {
        if self.len < 2 {
            return Ok(None);
        }
        let frame_len = u16::from_le_bytes([self.buf[0], self.buf[1]]) as usize;
        if frame_len == 0 || frame_len > MAX_FRAME_LEN || 2 + frame_len > N {
            self.len = 0;
            return Err(FrameError::TooLarge(frame_len));
        }
        if self.len < 2 + frame_len {
            return Ok(None);
        }
        let decoded = postcard::from_bytes::<DeviceMsg>(&self.buf[2..2 + frame_len]);
        let remaining = self.len - (2 + frame_len);
        self.buf.copy_within(2 + frame_len..self.len, 0);
        self.len = remaining;
        decoded.map(Some).map_err(|_| FrameError::Decode)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEvent<'a> {
    Line(&'a [u8]),
    Overflow,
}

/// Incremental `\n` line splitter for Port 8080 JSON uplink.
///
/// Discards `\r` and NUL bytes; marks oversized lines as `Overflow` and drops
/// bytes until the next `\n` so a huge line never splits into a bogus command.
pub struct LineSplitter<const N: usize> {
    buf: [u8; N],
    len: usize,
    overflowed: bool,
}

impl<const N: usize> Default for LineSplitter<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> LineSplitter<N> {
    pub const fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
            overflowed: false,
        }
    }

    pub fn push_byte(&mut self, b: u8) -> Option<LineEvent<'_>> {
        if b == b'\n' {
            let was_overflow = self.overflowed;
            let n = self.len;
            self.len = 0;
            self.overflowed = false;
            if was_overflow {
                return Some(LineEvent::Overflow);
            }
            if n > 0 {
                return Some(LineEvent::Line(&self.buf[..n]));
            }
            return None;
        }
        if b == 0 || b == b'\r' || self.overflowed {
            return None;
        }
        if self.len < N {
            self.buf[self.len] = b;
            self.len += 1;
        } else {
            self.overflowed = true;
            self.len = 0;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{default_profile, DEFAULT_SETTINGS};

    #[test]
    fn api_command_roundtrip_and_errors() {
        let (req_id, parsed) =
            parse_api_command(r#"{"cmd":"direct_pump","power":75.5,"req_id":9}"#).unwrap();
        assert_eq!(req_id, Some(9));
        assert_eq!(
            parsed,
            ParsedCommand::Machine(MachineCommand::DirectPump(75.5))
        );

        let (_, parsed) = parse_api_command(r#"{"cmd":"run_slot","slot":2}"#).unwrap();
        assert_eq!(parsed, ParsedCommand::RunSlot(2));

        assert_eq!(
            parse_api_command(r#"{"cmd":"run_slot","slot":10}"#),
            Err(CmdError::InvalidSlot)
        );
        assert_eq!(
            parse_api_command(
                r#"{"cmd":"save_profile","slot":10,"profile":{"name":"P","steps":[]}}"#
            ),
            Err(CmdError::InvalidSlot)
        );
        assert_eq!(
            parse_api_command(r#"{"cmd":"delete_profile","slot":10}"#),
            Err(CmdError::InvalidSlot)
        );
        assert_eq!(
            parse_api_command(r#"{"cmd":"get_profile","slot":10}"#),
            Err(CmdError::InvalidSlot)
        );
        assert_eq!(
            parse_api_command(r#"{"cmd":"direct_pump"}"#),
            Err(CmdError::MissingField)
        );
        assert_eq!(
            parse_api_command(r#"{"cmd":"unknown"}"#),
            Err(CmdError::UnknownCommand)
        );
        assert_eq!(parse_api_command("not json"), Err(CmdError::BadJson));
    }

    #[test]
    fn device_msg_frame_roundtrip_fragmented() {
        let mut fw = heapless::String::new();
        let _ = fw.push_str("0.2.0");
        let hello = DeviceMsg::Hello(DeviceHello {
            magic: DIAG_MAGIC,
            proto_ver: DIAG_PROTO_VER,
            fw_version: fw,
            settings: DEFAULT_SETTINGS.to_public(),
            profiles: heapless::Vec::new(),
            state: 0,
            sbt: 92.0,
        });
        let tick = DeviceMsg::Tick(TelemetryFrame {
            seq: 42,
            ms: 1234,
            t: 92.5,
            tt: 92.0,
            ett: 93.0,
            p: 8.9,
            tp: 9.0,
            fl: 2.1,
            tfl: 2.5,
            vol: 18.4,
            hp: 45.0,
            pump: 62.0,
            ac_us: 10000.0,
            sbt: 92.0,
            fc: 0,
            st: 1,
            step: 0,
            ack: 7,
        });
        let prof = DeviceMsg::ProfileData {
            slot: 0,
            profile: Some(default_profile()),
        };

        let mut wire = [0u8; 2048];
        let mut used = 0;
        for msg in [&hello, &tick, &prof] {
            let n = encode_device_msg(msg, &mut wire[used..]).unwrap();
            used += n;
        }

        let mut framer = RxFramer::<2048>::new();
        let mut decoded = Vec::new();
        for chunk in wire[..used].chunks(7) {
            let taken = framer.accept(chunk);
            assert_eq!(taken, chunk.len());
            while let Some(m) = framer.next_frame().unwrap() {
                decoded.push(m);
            }
        }
        assert_eq!(decoded, vec![hello, tick, prof]);
    }

    #[test]
    fn line_splitter_handles_cr_lf_and_overflow() {
        let mut sp = LineSplitter::<16>::new();
        let mut out = Vec::new();
        for &b in b"{\"cmd\":\"ping\"}\r\n{\"cmd\":\"0123456789abcdef\"}\n{\"cmd\":\"stop\"}\n" {
            if let Some(ev) = sp.push_byte(b) {
                match ev {
                    LineEvent::Line(l) => out.push(Ok(l.to_vec())),
                    LineEvent::Overflow => out.push(Err(())),
                }
            }
        }
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].as_ref().unwrap(), br#"{"cmd":"ping"}"#);
        assert!(out[1].is_err());
        assert_eq!(out[2].as_ref().unwrap(), br#"{"cmd":"stop"}"#);
    }

    #[test]
    fn password_is_redacted_in_public_settings() {
        let mut s = DEFAULT_SETTINGS.clone();
        let _ = s.wifi.ssid.push_str("CafeNet");
        let _ = s.wifi.password.push_str("super-secret");
        let pub_s = s.to_public();
        let json = serde_json::to_string(&pub_s).unwrap();
        assert!(!json.contains("super-secret"));
        assert!(json.contains("CafeNet"));
        assert!(json.contains("\"has_password\":true"));
    }
}
