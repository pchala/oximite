# Operations

This document describes machine setup, operational modes, and safety procedures.

## Initial provisioning and safe boot

1. **Flash firmware:** Flash the release binary to the Pico Plus 2 W via probe or UF2.
2. **Access Point mode:**
   - On first boot with unconfigured Wi-Fi, the machine starts as an open Access Point named `oximite-setup`.
   - Connect a smartphone or laptop, browse to `http://192.168.4.1`, and enter the local Wi-Fi SSID and password.
3. **Safe-boot override:**
   - If Wi-Fi credentials change or the network is unreachable, hold the **Right Toggle Switch UP** while you power on the machine (REQ-12).
   - The firmware ignores the stored credentials and starts in AP mode.

## Operational modes

The state coordinator manages machine states (`MachineState` enum):

| Mode | Trigger | Actuators active | Description |
|---|---|---|---|
| **Idle** | Left Toggle UP (power) / boot / finish | Heater PID active (brew target, 93 °C), pump off, valve closed | Maintains ready brew temperature. |
| **Brewing** | Right Toggle DOWN (brew) or Web UI | Heater PID active, pump modulated, valve open | Runs the selected multi-step extraction profile. |
| **Pumping / Flush** | Right Toggle UP (flush) or Web UI | Pump active (fixed 80% duty), valve open | Grouphead flush or direct pump. Safety ceiling 60 s. |
| **Steaming** | Left Toggle DOWN (steam) or Web UI | Heater PID active (steam setpoint, 135 °C), pump off, valve closed | Boosts boiler temperature for steam wand delivery. |
| **Cooling** | Coordinator after steam exit | Pump active, valve closed (flow to steam wand) | Cooldown flush to quickly lower boiler temperature back to brew level. |
| **Hot Water** | Web UI hot water command | Pump active, valve closed (flow through wand) | Dispenses hot water through the steam wand. Safety ceiling 120 s. |
| **Sleeping** | Left Toggle UP (power) or auto-sleep timer | Heater off, pump off, valve closed | Low-power idle state. Wakes on button press or UI command. |

## Status indication

Two addressable WS2812 RGB LEDs are chained on GP9 (`led0` and `led1`), clocked out by PIO2 SM1 at 10 Hz:

### Color crossfade model (`temp_color`)
Colors smoothly interpolate based on measurement vs target within a $\pm\text{window}$ span:
- **Blue:** Below target (cold / under-powered).
- **Green:** On target setpoint ($\pm\text{window}$ crossfade).
- **Red:** Above target (hot / over-powered).

### Dual LED display modes

| State | LED 0 (Boiler status) | LED 1 (Auxiliary / Process status) |
|---|---|---|
| **Sleeping** | Solid Magenta (`RGB(255, 0, 255)`) | Solid Magenta (`RGB(255, 0, 255)`) |
| **Warmup** (<6 min from wake while Idle) | Boiler temp vs brew target (Blue $\to$ Green $\to$ Red, $\pm 10\text{ }^\circ\text{C}$) | Red brightness proportional to heater duty (0–100%) |
| **Idle** (after 6 min warmup) | Boiler temp vs brew target (Solid Green when ready) | Off |
| **Brewing** | Boiler temp vs brew target | Pump duty vs 80% target duty (Blue $\le 60\%$, Green at 80%, Red at 100%) |
| **Steaming** | Off | Boiler temp vs steam target (135 °C) |

## Thermal calibration and PID tuning

Configure PID gains through the web dashboard or telemetry command socket:
- **Kp (Proportional):** Sets heating power proportional to temperature error.
- **Ki (Integral):** Eliminates steady-state offset.
- **Kd (Derivative):** Dampens overshoot on warm-up.

Default baseline values are tuned for a standard brass boiler (~300 ml) with external NTC thermistor.
