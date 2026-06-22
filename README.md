# oximite

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/J5E121FXSB)

**oximite** is a high-performance, asynchronous Rust firmware for the Raspberry Pi Pico W, designed to retrofit standard espresso machines (such as the Gaggia Classic or Rancilio Silvia) with advanced digital controls.

By replacing the factory internals with oximite, machines gain capabilities typically found in commercial or prosumer equipment, including real-time pressure profiling, PID temperature control, volumetric dosing, and a web-based user interface natively hosted on the Pico W.

Support the continued development of this open-source project by [leaving a tip on Ko-fi](https://ko-fi.com/J5E121FXSB). Contributions help accelerate the implementation of new features.

## Features

*   **PID Temperature Control:** Configurable Kp, Ki, and Kd parameters for both brewing and steaming. Controls the boiler via Solid State Relays (SSR) to maintain thermal stability.
*   **Pressure Profiling & Flow Control:** Advanced pump modulation via phase-angle triac firing, hardware-synced to 50/60Hz AC mains (zero-cross detection). Supports dynamic adjustment of target pressure and flow rate (ml/s).
*   **Volumetric Dosing:** Accurate dose measurement utilizing a Hall-effect flow meter.
*   **Live Web Dashboard:** An interactive web interface served directly over Wi-Fi. Features live telemetry graphing (pressure, flow, volume), machine controls (brew, flush, steam, descale), and an integrated settings manager.
*   **Custom Brew Profiles:** Create, save, and execute multi-step extraction profiles targeting time, volume, pressure, or flow. Up to 10 profiles can be persisted in the Pico's flash memory.
*   **Telemetry & Command Socket:** An open TCP command socket streams high-granularity telemetry and accepts remote commands. This enables automated testing out-of-the-box and serves as a backend for custom companion apps, high-resolution graphing tools, and unlimited profile sharing.

## Architecture

The firmware is written in Rust and utilizes the `embassy-rp` asynchronous framework to distribute workloads across the RP2040's dual cores:

*   **Core 0 (I/O & Control):** Handles hard-real-time I/O tasks. It runs PIO state machines for zero-cross detection, triac firing, flow meter pulse counting, hardware ADC sampling, and acts as the main state coordinator.
*   **Core 1 (Networking):** Dedicated to the CYW43 Wi-Fi driver, the TCP/IP networking stack, and serving the embedded HTTP Web Server.

## Pin Assignments

| Pin / GPIO | Peripheral / Name | Direction | Description |
| :--- | :--- | :--- | :--- |
| **GP0** | PIO0 (SM2) | Output | **Triac Control:** Phase-angle firing for pump/heater modulation. |
| **GP2** | Standard GPIO | Output | **Heater Relay:** Solid State Relay (SSR) control for the boiler. |
| **GP3** | Standard GPIO | Output | **3-Way Valve:** Solid State Relay (SSR) control for the brew group solenoid. |
| **GP9** | PIO0 (SM3) | Output | **WS2812 RGB LED:** Status indication via addressable LEDs. |
| **GP10** | PIO0 (SM1) | Input | **Zero-Cross:** Syncs Triac firing with 50/60Hz AC mains. |
| **GP6** | GPIO (Pull-Up) | Input | **Brew Button:** Physical button to start/stop brewing. |
| **GP7** | GPIO (Pull-Up) | Input | **Steam Button:** Physical button to toggle steam mode. |
| **GP8** | GPIO (Pull-Up) | Input | **Flush Button:** Physical button for quick grouphead flush. |
| **GP15** | PIO0 (SM0) | Input | **Flow Meter:** Reads pulses from a Hall-effect water flow sensor. |
| **GP23** | WL_ON | Output | *Internal:* CYW43 Wi-Fi chip power control. |
| **GP24** | WL_D / PIO1 | In/Out | *Internal:* CYW43 Wi-Fi SPI Data. |
| **GP25** | WL_CS | Output | *Internal:* CYW43 Wi-Fi Chip Select. |
| **GP26** | ADC Channel 0 | Analog In | **Pressure Sensor:** Reads analog voltage from the pressure transducer. |
| **GP27** | ADC Channel 1 | Analog In | **Temp Sensor:** Reads analog voltage from the thermistor/thermocouple. |
| **GP29** | WL_CLK / PIO1 | Output | *Internal:* CYW43 Wi-Fi SPI Clock. |

## Getting Started

### 1. Build & Flash

Ensure the Rust `thumbv6m-none-eabi` target is installed. The firmware is standalone and includes the `BOOT2` stage.

```bash
cargo run --release
```

### 2. Automated Tests

To execute the automated integration tests over the network:

```bash
cd tests
uv run python -m unittest test_oximite.py
```

## Production Optimizations

The `release` profile is optimized for performance and stability:

*   `opt-level = 3`
*   `lto = "fat"` (Link Time Optimization)
*   `codegen-units = 1`
*   `panic = "abort"`
*   Debug symbols are kept (`debug = 2`) as `defmt` relies on them for formatting logs over the USB/RTT connection.

## Disclaimer

All hardware designs, code, and documentation are provided "AS IS", without warranty of any kind, express or implied. The author(s) assume no responsibility or liability for any personal injury, property damage, or other losses that may occur from building, using, or attempting to replicate this project. You are solely responsible for your own safety and the safety of your equipment.
