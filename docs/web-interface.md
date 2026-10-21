# Web interface and HTTP API

This document specifies the embedded HTTP web server and REST API running on port 80.

## Overview

Core 1 runs an asynchronous HTTP server using `embassy-net`. It serves the single-page application dashboard from flash (`firmware/html/index.html`) and provides REST endpoints for browser control and configuration.

## Endpoints

### 1. Static dashboard

- **`GET /`** or **`GET /index.html`**  
  Returns the compressed HTML, JavaScript, and CSS bundle containing the live graphing interface.

### 2. State and telemetry

- **`GET /api/telemetry`**  
  Returns the current system telemetry snapshot as JSON:
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
  Fields: `t` (grouphead temp), `sbt` (session brew temp setpoint), `p` (pressure), `fl` (flow rate), `vol` (volume), `st` (machine state, see `MachineState` in [Firmware design](firmware.md)), `ack` (command ticket acknowledged).

### 3. Machine control

- **`POST /api/cmd`**  
  Dispatches an operational command:
  ```json
  {"cmd": "run_slot", "slot": 0}
  ```
  Supported commands: `"brew"`, `"stop"`, `"flush"`, `"steam"`, `"power"`, `"direct_pump"`, `"run_slot"`, `"profile"`, `"set_session_temp"`, `"save_settings"`, `"save_profile"`, `"delete_profile"`.

### 4. Settings management

- **`GET /api/settings`**  
  Retrieves current operational parameters.
- **`POST /api/settings`**  
  Updates settings in RAM and commits to flash via `sequential-storage`:
  ```json
  {
    "brew_temp": 93.5,
    "steam_temp": 135.0,
    "pid": {"kp": 10.5, "ki": 0.35, "kd": 22.0}
  }
  ```

### 5. Extraction profiles

- **`GET /api/profiles`**  
  Returns all persisted multi-step profiles. REQ-16 sets the limit.
- **`POST /api/profiles`**  
  Saves or overwrites a profile definition.
