# Telemetry and command socket

This document specifies the bidirectional TCP protocol on port 8080.

## Overview

Port 8080 opens a single streaming connection for telemetry recording, automated testing, and companion applications.
- **Downlink (Controller -> Client):** One fixed-size binary header on connection, followed by one 55-byte [postcard](https://github.com/jamesmunns/postcard) binary record per control tick (50 Hz).
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

Each 50 Hz tick emits a serialized binary frame:

| Offset | Field | Type | Description |
|---|---|---|---|
| 0 | `ver` | `u8` | Frame protocol version |
| 1 | `seq` | `u32` | Cumulative control-loop tick counter |
| 5 | `ms` | `u32` | Device uptime in milliseconds |
| 9 | `t` | `f32` | Raw boiler temperature (°C) |
| 13 | `tt` | `f32` | Target temperature (°C) |
| 17 | `ett` | `f32` | Effective temperature setpoint with flow feed-forward (°C) |
| 21 | `p` | `f32` | Measured grouphead pressure (bar) |
| 25 | `tp` | `f32` | Target pressure (bar) |
| 29 | `fl` | `f32` | Measured flow rate (ml/s) |
| 33 | `tfl` | `f32` | Target flow rate (ml/s) |
| 37 | `vol` | `f32` | Cumulative shot volume (ml) |
| 41 | `hp` | `f32` | Boiler heating power ratio (0.0 to 1.0) |
| 45 | `pump` | `f32` | Pump triac duty cycle (0.0 to 100.0) |
| 49 | `fc` | `u8` | Flow limit active constraint flag (0 or 1) |
| 50 | `st` | `u8` | Machine state (0=Idle, 1=Brewing, 2=Steaming, 3=Sleeping, 4=Pumping, 5=Cooling, 6=HotWater) |

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
