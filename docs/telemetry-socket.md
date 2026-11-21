# Telemetry and command socket

This document specifies the bidirectional TCP protocol on port 8080 (`diag_task.rs` + `proto/src/protocol.rs`).

## Overview

Port 8080 accepts a single streaming client connection (used by `oximite-control` via direct IP or hostname) for 50 Hz telemetry, typed events, configuration inspection, and control commands.

- **Downlink (Controller -> Client):** Length-prefixed binary [postcard](https://github.com/jamesmunns/postcard) v3 frames (`[len: u16 LE][postcard DeviceMsg]`). Maximum payload length (`MAX_FRAME_LEN`) is 1024 bytes.
- **Uplink (Client -> Controller):** Newline-delimited JSON commands (`ApiCommand`, compatible with Port 80 `POST /api/cmd`).

## Downlink protocol (`DeviceMsg`)

Upon client connection, the controller sends `DeviceMsg::Hello(DeviceHello)` before streaming telemetry ticks.

### Handshake (`DeviceHello`)

| Field | Type | Description |
| --- | --- | --- |
| `magic` | `u32` | Protocol magic constant (`0x4D49_584F` / `"OXIM"`) |
| `proto_ver` | `u8` | Wire protocol version (`3`) |
| `fw_version` | `String<16>` | Firmware package version string |
| `settings` | `PublicSettings` | Machine, PID, and redacted Wi-Fi snapshot |
| `profiles` | `Vec<ProfileHeaderOwned, 10>` | List of occupied profile slots and names |
| `state` | `u8` | Current `MachineState` numeric wire value |
| `sbt` | `f32` | Session brew temperature setpoint (°C) |

### Message envelopes (`DeviceMsg`)

- `Hello(DeviceHello)`: Initial session snapshot pushed immediately on TCP accept.
- `Tick(TelemetryFrame)`: Periodic control telemetry sample (50 Hz default, configurable via `set_stream`).
- `CommandReply { req_id, ticket, err }`: Acknowledgement or `CmdError` for uplink commands that supply `req_id`.
- `SettingsSnapshot(PublicSettings)`: Response to `get_settings` or push on settings revision change.
- `ProfilesList(Vec<ProfileHeaderOwned, 10>)`: Response to `get_profiles` or push on profile revision change.
- `ProfileData { slot, profile }`: Response to `get_profile` (`Option<BrewProfile>`).
- `Event(DeviceEvent)`: Real-time state machine or storage notification (`StateChanged`, `StepChanged`, `SettingsUpdated`, `ProfilesUpdated`, `FlashSaved`).
- `Pong { req_id, uptime_ms }`: Latency probe response for `ping`.

### Telemetry sample (`TelemetryFrame`)

See `TelemetryFrame` in `proto/src/protocol.rs` for exact serialization order:

| Field | Type | Description |
| --- | --- | --- |
| `seq` | `u32` | Cumulative control-loop tick counter |
| `ms` | `u32` | Device uptime in milliseconds |
| `t` | `f32` | Measured boiler temperature (°C) |
| `tt` | `f32` | Target boiler temperature (°C) |
| `ett` | `f32` | Effective temperature setpoint with flow feed-forward (°C) |
| `p` | `f32` | Measured grouphead pressure (bar) |
| `tp` | `f32` | Target pressure (bar) |
| `fl` | `f32` | Measured flow rate (ml/s) |
| `tfl` | `f32` | Target flow rate (ml/s) |
| `vol` | `f32` | Cumulative shot volume (ml) |
| `hp` | `f32` | Boiler heating power duty (0.0 to 100.0 %) |
| `pump` | `f32` | Pump triac duty cycle (0.0 to 100.0 %) |
| `ac_us` | `f32` | Filtered AC half-wave duration (µs) |
| `sbt` | `f32` | Session brew temperature setpoint (°C) |
| `fc` | `u8` | Flow-limiting active constraint flag (0 or 1) |
| `st` | `u8` | Machine state (see `MachineState` in [Firmware design](firmware.md)) |
| `step` | `u8` | Active profile step index (`255` when none) |
| `ack` | `u32` | Last completed command ticket |

## Uplink JSON commands (`ApiCommand`)

Commands MUST terminate with `\n`. Every command MAY include an optional `"req_id": <u32>` to request a correlated `CommandReply` or `Pong`. Uplink accepts all Port 80 `POST /api/cmd` actions plus socket-only read/stream queries:

```json
{"cmd": "ping", "req_id": 1}
{"cmd": "set_stream", "req_id": 2, "enabled": true, "divider": 1}
{"cmd": "get_settings"}
{"cmd": "get_profiles"}
{"cmd": "get_profile", "slot": 0}
{"cmd": "run_slot", "req_id": 3, "slot": 0}
{"cmd": "stop", "req_id": 4}
{"cmd": "set_session_temp", "temp": 93.5}
```
