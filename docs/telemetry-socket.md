# Telemetry and command socket

This document specifies the bidirectional TCP protocol on port 8080.

## Overview

Port 8080 opens a single streaming connection for telemetry recording, automated testing, and companion applications.
- **Downlink (Controller -> Client):** One fixed-size binary header on connection, followed by one [postcard](https://github.com/jamesmunns/postcard) binary record per control tick (50 Hz).
- **Uplink (Client -> Controller):** Newline-delimited JSON commands parsed asynchronously.

## Connection handshake

Upon client connection, the controller sends an initial diagnostic header before streaming frames:

| Field | Type | Description |
|---|---|---|
| `magic` | `u32` | Protocol magic constant (`0x4449584F` / `"OXID"`) |
| `ver` | `u8` | Telemetry wire-format version (currently `2`) |
| `frame_len` | `u8` | Serialized length of each frame in bytes |
| `temp_offset` | `f32` | Boiler-to-grouphead temperature offset (°C) |
| `brew_temp` | `f32` | Base brew temperature setpoint (°C) |
| `steam_temp`| `f32` | Base steam temperature setpoint (°C) |
| `temp_kp` | `f32` | Temperature PID proportional gain |
| `temp_ki` | `f32` | Temperature PID integral gain |
| `temp_kd` | `f32` | Temperature PID derivative gain |
| `pump_kp` | `f32` | Pump pressure PID proportional gain |
| `pump_ki` | `f32` | Pump pressure PID integral gain |
| `pump_kd` | `f32` | Pump pressure PID derivative gain |

## Telemetry frame layout

Each 50 Hz tick emits one postcard-serialized frame. The table lists the fields in serialization order. The firmware frame struct (`DiagFrame` in `web_api.rs`) defines the encoded size. Read the size from the `frame_len` field of the handshake header.

| Field | Type | Description |
|---|---|---|
| `ver` | `u8` | Frame protocol version |
| `seq` | `u32` | Cumulative control-loop tick counter |
| `ms` | `u32` | Device uptime in milliseconds |
| `t` | `f32` | Raw boiler temperature (°C) |
| `tt` | `f32` | Target temperature (°C) |
| `ett` | `f32` | Effective temperature setpoint with flow feed-forward (°C) |
| `p` | `f32` | Measured grouphead pressure (bar) |
| `tp` | `f32` | Target pressure (bar) |
| `fl` | `f32` | Measured flow rate (ml/s) |
| `tfl` | `f32` | Target flow rate (ml/s) |
| `vol` | `f32` | Cumulative shot volume (ml) |
| `hp` | `f32` | Boiler heating power ratio (0.0 to 1.0) |
| `pump` | `f32` | Pump triac duty cycle (0.0 to 100.0) |
| `fc` | `u8` | Flow limit active constraint flag (0 or 1) |
| `st` | `u8` | Machine state (see `MachineState` in [Firmware design](firmware.md)) |

## Uplink JSON commands

Commands must end with `\n`. Example commands:

```json
{"cmd": "run_slot", "slot": 0}
{"cmd": "stop"}
{"cmd": "flush"}
{"cmd": "steam"}
{"cmd": "power"}
{"cmd": "direct_pump", "power": 80.0}
{"cmd": "set_session_temp", "temp": 93.5}
```
