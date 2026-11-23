# Operations

This document describes machine setup, operational modes, status LEDs, and PID tuning.

## Initial provisioning and safe boot

1. **Flash firmware:** Flash the release binary to the Pico Plus 2 W via probe or UF2.
2. **Access Point mode:**
   - On first boot with unconfigured Wi-Fi, the machine starts a WPA2 Access Point named `Oximite-Setup` (password `password`) with DHCP active.
   - Connect a smartphone or laptop, browse to `http://192.168.4.1`, and save the local Wi-Fi SSID and password.
3. **Safe-boot override:**
   - If Wi-Fi credentials change or the network is unreachable, hold the **Flush button** while you power on the machine (REQ-12).
   - The firmware ignores stored credentials and starts in AP mode.

## Operational modes

The state coordinator (`machine.rs` + `coordinator_task.rs`) manages machine states (`MachineState` enum):

| Mode | Trigger | Actuators active | Description |
| --- | --- | --- | --- |
| **Idle** | Power button / boot / finish | Heater PID active (brew target), pump off, valve closed | Maintains ready brew temperature. |
| **Brewing** | Brew button / Web UI / Desktop app | Heater PID active, pump modulated, valve open | Runs the selected multi-step extraction profile. |
| **Pumping / Flush** | Flush button / Web UI / Desktop app | Pump active (fixed 80% duty), valve open | Grouphead flush or direct pump. Safety ceiling 60 s. |
| **Steaming** | Steam button / Web UI / Desktop app | Heater PID active (steam setpoint), pump off, valve closed | Boosts boiler temperature for steam wand delivery. |
| **Cooling** | Coordinator after steam exit | Pump active, valve closed (flow to steam wand) | Cooldown flush to lower boiler temperature back to brew level. |
| **Hot Water** | Reserved operational state | Pump active, valve closed | Controlled water delivery state with 120 s ceiling. |
| **Sleeping** | Power button or auto-sleep timer | Heater off, pump off, valve closed | Low-power idle state. Wakes on switch input or operational UI command. |

Note: Persistent configuration updates (`save_machine`, `save_pids`, `save_wifi`, `save_profile`, `delete_profile`) sent while the machine is busy defer both RAM activation and flash commit until the machine returns to an idle state. Session brew temperature (`set_session_temp`) applies immediately in RAM.

## Status indication

Two addressable WS2812 RGB LEDs are chained on GP9 (`led0` and `led1`), computed by pure `leds.rs` and clocked out by `led_task.rs` on PIO2 SM1 at 10 Hz:

### Color crossfade model (`temp_color`)

Colors interpolate smoothly between measurement and target within a +/-`window` span:
- **Blue:** Below target (cold / under-powered).
- **Green:** On target setpoint.
- **Red:** Above target (hot / over-powered).

### Dual LED display modes

| State | LED 0 (Boiler status) | LED 1 (Auxiliary / Process status) |
| --- | --- | --- |
| **Sleeping** | Solid Magenta (`RGB(255, 0, 255)`) | Solid Magenta (`RGB(255, 0, 255)`) |
| **Warmup** (<6 min from wake while Idle/non-busy) | Boiler temp vs brew target (Blue -> Green -> Red, +/-10 °C) | Red brightness proportional to heater duty (0–100%) |
| **Idle** (after 6 min warmup) | Boiler temp vs brew target | Off |
| **Brewing** | Boiler temp vs brew target | Pump duty vs 80% target (+/-20% window) |
| **Steaming** | Off | Boiler temp vs steam target |

## Thermal calibration and PID tuning

Use the native desktop companion application `oximite-control` (`control/`) for PID tuning and diagnostics. Connect by device IP address or hostname. The app sends commands over HTTP port 80 and reads live telemetry from TCP port 8080 to access synchronized 3-panel live graphs, live PID and machine gain editing, multi-step profile management, automated heater/pump bench sweeps, shot recording/comparison overlays, and the timestamped event log.

The embedded web UI on port 80 remains available for lightweight shot triggering, session brew temperature tweaks, and initial Wi-Fi credential provisioning without external internet access.
