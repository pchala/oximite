# Web interface and HTTP API

This document specifies the embedded HTTP web server and JSON endpoints on TCP port 80.

## Overview

Core 1 runs an asynchronous HTTP/1.1 server (`http_task.rs` + pure `http_codec.rs`) on `embassy-net`. It serves a lightweight, offline, graph-free control and Wi-Fi setup page (`firmware/html/index.html`) and JSON endpoints for browser control. For live multi-panel graphing, PID tuning, and bench sweeps, use the native desktop app (`control/`).

## Endpoints

### 1. Static dashboard

- **`GET /`** or **`GET /index.html`**  
  Returns the gzip-compressed HTML, JavaScript, and CSS bundle. The dashboard polls `GET /api/telemetry` every 500 ms and contains no external CDN assets or live charts.

### 2. State and telemetry

- **`GET /api/telemetry`**  
  Returns the current system snapshot (`UiTelemetry` in `proto/src/protocol.rs`):
  ```json
  {
    "t": 93.2,
    "sbt": 93.5,
    "p": 9.1,
    "fl": 2.1,
    "vol": 36.4,
    "st": 1,
    "ack": 42
  }
  ```
  Fields: `t` (displayed temp in °C), `sbt` (session brew target setpoint in °C), `p` (pressure in bar), `fl` (flow rate in ml/s), `vol` (volume in ml), `st` (`MachineState` numeric wire value), `ack` (last completed command ticket).

### 3. Machine control and settings updates

- **`POST /api/cmd`**  
  Accepts a JSON command matching `ApiCommand` (`proto/src/protocol.rs`) and returns `{"status":"ok","ack":<u32>}` on acceptance:
  ```json
  {"cmd": "run_slot", "slot": 0}
  ```
  Supported HTTP `cmd` actions:
  - Operational: `"power"`, `"brew"`, `"stop"`, `"steam"`, `"flush"`, `"direct_pump"` (`power`), `"profile"` (`profile`), `"run_slot"` (`slot`).
  - Session adjustment (RAM only): `"set_session_temp"` (`temp`).
  - Persistent configuration (`Ambient::RamAndFlash`, deferred until idle): `"save_machine"` (`machine`), `"save_pids"` (`temp_pid`, `pump_pid`), `"save_wifi"` (`wifi`), `"save_profile"` (`slot`, `profile`), `"delete_profile"` (`slot`).

### 4. Read-only configuration queries

- **`GET /api/version`**  
  Returns `{"proto": N}`, the port 8080 wire format version. The control app compares it with its own `PROTO_VER` and warns on a mismatch.
- **`GET /api/settings`**  
  Returns `PublicSettings` (`machine`, `temp_pid`, `pump_pid`, and `wifi: {"ssid": "...", "has_password": bool}`). Stored Wi-Fi passwords are never returned over the network.
- **`GET /api/profiles`**  
  Returns saved profile slot headers as a JSON array (`[{"slot": 0, "name": "Standard"}, ...]`). REQ-16 sets the slot limit.
- **`GET /api/profile/<slot>`**  
  Returns the full `BrewProfile` JSON object for slot `0..=9`, or `404 Not Found` if the slot is empty or out of range.
