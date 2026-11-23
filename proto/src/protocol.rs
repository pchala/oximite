//! Wire protocol codecs shared by HTTP (Port 80) and the TCP stream (Port 8080).
//!
//! - Commands and queries use Port 80 only. `POST /api/cmd` takes an
//!   [`ApiCommand`] parsed by [`parse_api_command`].
//! - Port 8080 is one-way telemetry. It sends length-prefixed Postcard
//!   frames `[len: u16 LE][postcard DeviceMsg]`, encoded with
//!   [`encode_device_msg`] and decoded incrementally with [`RxFramer`].

use serde::{Deserialize, Serialize};

use crate::model::{
    BrewProfile, MachineCommand, MachineSettings, PidSettings, Telemetry, WifiSettings,
    MAX_PROFILES,
};

pub const MAX_FRAME_LEN: usize = 128;

/// Port 8080 wire format version, reported by `GET /api/version`.
pub const PROTO_VER: u8 = 4;

#[derive(Serialize)]
pub struct VersionInfo {
    pub proto: u8,
}

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

/// JSON command schema of Port 80 `POST /api/cmd`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ApiCommand<'a> {
    pub cmd: &'a str,
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
}

impl<'a> ApiCommand<'a> {
    pub fn simple(cmd: &'a str) -> Self {
        Self {
            cmd,
            ..Default::default()
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum ParsedCommand {
    Machine(MachineCommand),
    RunSlot(u8),
}

fn req<T>(v: Option<T>) -> Result<T, CmdError> {
    v.ok_or(CmdError::MissingField)
}

fn req_slot(slot: Option<u8>) -> Result<u8, CmdError> {
    let slot = req(slot)?;
    if slot >= MAX_PROFILES {
        return Err(CmdError::InvalidSlot);
    }
    Ok(slot)
}

pub fn parse_api_command_struct(payload: ApiCommand<'_>) -> Result<ParsedCommand, CmdError> {
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
        "run_slot" => ParsedCommand::RunSlot(req_slot(payload.slot)?),
        "save_profile" => {
            let slot = req_slot(payload.slot)?;
            ParsedCommand::Machine(MachineCommand::SaveProfile(slot, req(payload.profile)?))
        }
        "delete_profile" => {
            ParsedCommand::Machine(MachineCommand::DeleteProfile(req_slot(payload.slot)?))
        }
        _ => return Err(CmdError::UnknownCommand),
    };
    Ok(parsed)
}

pub fn parse_api_command(json: &str) -> Result<ParsedCommand, CmdError> {
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

/// Decoded telemetry sample. The app builds it from [`WireTick`].
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TelemetryFrame {
    pub seq: u32,
    pub ms: u32,
    pub t: f32,
    pub tt: f32,
    pub p: f32,
    pub tp: f32,
    pub fl: f32,
    pub tfl: f32,
    pub vol: f32,
    pub hp: f32,
    pub pump: f32,
    pub sbt: f32,
    pub fc: u8,
    pub st: u8,
    pub step: u8,
}

const FLAG_FLOW_CONTROLLED: u8 = 0x80;

/// Compact fixed-point tick sent once per mains cycle inside `DeviceMsg::Tick`.
///
/// Units: temperatures, pressures and flows use 0.01, volume uses 0.1 ml,
/// duties use 1 percent. `seq` wraps at 256. `dt_us` is the mains period,
/// which is the time since the previous tick. `flags` holds the machine
/// state in bits 0 to 6 and the flow-controlled bit in bit 7.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireTick {
    pub seq: u8,
    pub dt_us: u16,
    pub t: u16,
    pub tt: u16,
    pub p: u16,
    pub tp: u16,
    pub fl: u16,
    pub tfl: u16,
    pub vol: u16,
    pub hp: u8,
    pub pump: u8,
    pub sbt: u16,
    pub flags: u8,
    pub step: u8,
}

/// Float to fixed point. Casts saturate, so negative and NaN values become 0.
fn fixed(v: f32, scale: f32) -> u16 {
    (v * scale + 0.5) as u16
}

impl WireTick {
    pub fn from_telemetry(t: &Telemetry, sbt: f32, state: u8, step: u8) -> Self {
        let flow_flag = if t.flow_controlled {
            FLAG_FLOW_CONTROLLED
        } else {
            0
        };
        Self {
            seq: t.tick as u8,
            dt_us: t.ac_period_us as u16,
            t: fixed(t.temp_c, 100.0),
            tt: fixed(t.target_temp, 100.0),
            p: fixed(t.pressure_bar, 100.0),
            tp: fixed(t.target_bar, 100.0),
            fl: fixed(t.flow_rate_ml_s, 100.0),
            tfl: fixed(t.target_ml_s, 100.0),
            vol: fixed(t.volume_ml, 10.0),
            hp: (t.heater_duty + 0.5) as u8,
            pump: (t.pump_duty + 0.5) as u8,
            sbt: fixed(sbt, 100.0),
            flags: (state & !FLAG_FLOW_CONTROLLED) | flow_flag,
            step,
        }
    }

    pub fn state(&self) -> u8 {
        self.flags & !FLAG_FLOW_CONTROLLED
    }

    pub fn flow_controlled(&self) -> bool {
        self.flags & FLAG_FLOW_CONTROLLED != 0
    }

    /// Expands to floats. `seq` and `ms` come from the receiver, which
    /// unwraps `seq` and sums `dt_us`.
    pub fn to_frame(&self, seq: u32, ms: u32) -> TelemetryFrame {
        TelemetryFrame {
            seq,
            ms,
            t: f32::from(self.t) / 100.0,
            tt: f32::from(self.tt) / 100.0,
            p: f32::from(self.p) / 100.0,
            tp: f32::from(self.tp) / 100.0,
            fl: f32::from(self.fl) / 100.0,
            tfl: f32::from(self.tfl) / 100.0,
            vol: f32::from(self.vol) / 10.0,
            hp: f32::from(self.hp),
            pump: f32::from(self.pump),
            sbt: f32::from(self.sbt) / 100.0,
            fc: u8::from(self.flow_controlled()),
            st: self.state(),
            step: self.step,
        }
    }
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
pub enum DeviceMsg {
    Tick(WireTick),
    Event(DeviceEvent),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DEFAULT_SETTINGS;

    #[test]
    fn api_command_roundtrip_and_errors() {
        let parsed = parse_api_command(r#"{"cmd":"direct_pump","power":75.5}"#).unwrap();
        assert_eq!(
            parsed,
            ParsedCommand::Machine(MachineCommand::DirectPump(75.5))
        );

        let parsed = parse_api_command(r#"{"cmd":"run_slot","slot":2}"#).unwrap();
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
        let tick = DeviceMsg::Tick(sample_tick());
        let event = DeviceMsg::Event(DeviceEvent::SettingsUpdated);

        let mut wire = [0u8; 2048];
        let mut used = 0;
        for msg in [&tick, &event] {
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
        assert_eq!(decoded, vec![tick, event]);
    }

    fn sample_tick() -> WireTick {
        WireTick {
            seq: 42,
            dt_us: 10_000,
            t: 9250,
            tt: 9200,
            p: 890,
            tp: 900,
            fl: 210,
            tfl: 250,
            vol: 184,
            hp: 45,
            pump: 62,
            sbt: 9200,
            flags: 0x81,
            step: 3,
        }
    }

    #[test]
    fn wire_tick_is_small_and_expands() {
        let mut buf = [0u8; 64];
        let n = encode_device_msg(&DeviceMsg::Tick(sample_tick()), &mut buf).unwrap();
        assert!(n <= 30, "tick frame is {n} bytes");

        let f = sample_tick().to_frame(7, 500);
        assert_eq!((f.seq, f.ms, f.st, f.fc, f.step), (7, 500, 1, 1, 3));
        assert!((f.t - 92.5).abs() < 1e-3);
        assert!((f.vol - 18.4).abs() < 1e-3);
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
