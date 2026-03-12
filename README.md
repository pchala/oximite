# Oximite - Smart Espresso Machine Controller ☕

Oximite is a high-performance, asynchronous Rust firmware for the Raspberry Pi Pico W. It retrofits standard espresso machines (like the Gaggia Classic) with advanced features including PID temperature control, pressure profiling, volumetric dosing, and a live web interface.

## ✨ Key Features

*   **🌡️ Advanced PID Temperature Control:** Configurable Kp, Ki, Kd parameters for both brewing and steaming. Includes temperature offset adjustments and Solid State Relay (SSR) control for the boiler.
*   **🎯 Pressure Profiling & Flow Control:** Advanced pump control via phase-angle triac firing synced to 50/60Hz AC mains (zero-cross detection). Supports target pressure and flow rate (ml/s) tracking.
*   **💧 Volumetric Dosing:** Accurate dose measurement using a Hall-effect flow meter.
*   **💻 Live Web Interface:** Interactive dashboard served over Wi-Fi from the Pico W. Features real-time telemetry graphing (pressure, flow, volume), direct machine control (brew, flush, steam, descale), and an intuitive settings manager.
*   **📈 Custom Brew Profiles:** Create, save, and execute multi-step extraction profiles (targeting time, volume, pressure, and flow) directly from the web interface. Settings and up to 10 profiles are persisted to flash memory.
*   **🧪 HIL Testing & Simulation:** Hardware-In-the-Loop simulation modes for safe development, generating simulated mains zero-crossings and flow pulses. Exposes a 2Mbit/s UART JSON-L stream for Python-driven integration tests.

## ⚙️ Architecture

The system utilizes the `embassy-rp` asynchronous framework to distribute workloads across both cores of the RP2040:
* **Core 0:** Handles hard-real-time I/O, including PIO state machines for zero-cross detection, triac firing, flow meter pulse counting, and hardware ADC sampling, as well as the main coordinator logic.
* **Core 1:** Handles networking (CYW43 Wi-Fi driver, TCP/IP stack) and the embedded HTTP Web Server.
* **HIL Testing:** Includes a 2Mbit/s UART interface for Hardware-In-the-Loop (HIL) testing via Python.

## 📍 Pin Assignments

| Pin / GPIO | Peripheral / Name | Direction | Description |
| :--- | :--- | :--- | :--- |
| **GP0** | PIO0 (SM2) | Output | **Triac Control:** Phase-angle firing for pump/heater modulation. |
| **GP2** | Standard GPIO | Output | **Heater Relay:** Solid State Relay (SSR) control for the boiler. |
| **GP4** | UART1 TX | Output | **HIL Telemetry:** 2Mbit/s JSON-L stream for Python testing. |
| **GP5** | UART1 RX | Input | **HIL Commands:** Receives JSON-L commands from Python. |
| **GP9** | PIO1 (SM1) | Output | **WS2812 RGB LED:** Status indication via addressable LEDs. |
| **GP10** | PIO0 (SM1) | Input | **Zero-Cross:** Syncs Triac firing with 50/60Hz AC mains. |
| **GP6** | GPIO (Pull-Up) | Input | **Brew Button:** Physical button to start/stop brewing. |
| **GP7** | GPIO (Pull-Up) | Input | **Steam Button:** Physical button to toggle steam mode. |
| **GP8** | GPIO (Pull-Up) | Input | **Flush Button:** Physical button for quick grouphead flush. |
| **GP15** | PIO0 (SM0) | Input | **Flow Meter:** Reads pulses from a Hall-effect water flow sensor. |
| **GP16** | PWM0 (Slice 0) | Output | **Flow Sim:** Hardware PWM to simulate flow meter pulses (HIL Only). |
| **GP18** | PWM1 (Slice 1) | Output | **Mains Sim:** Hardware PWM to simulate AC 50Hz zero-crossings (HIL Only). |
| **GP23** | WL_ON | Output | *Internal:* CYW43 Wi-Fi chip power control. |
| **GP24** | WL_D / PIO1 | In/Out | *Internal:* CYW43 Wi-Fi SPI Data. |
| **GP25** | WL_CS | Output | *Internal:* CYW43 Wi-Fi Chip Select. |
| **GP26** | ADC Channel 0 | Analog In | **Pressure Sensor:** Reads analog voltage from the pressure transducer. |
| **GP27** | ADC Channel 1 | Analog In | **Temp Sensor:** Reads analog voltage from the thermistor/thermocouple. |
| **GP29** | WL_CLK / PIO1 | Output | *Internal:* CYW43 Wi-Fi SPI Clock. |

## 🚀 Getting Started

### 1. Build & Flash the Firmware
Ensure you have the Rust `thumbv6m-none-eabi` target installed. The firmware is standalone and includes its own `BOOT2` stage.

#### Build Variants
The firmware can be compiled with different feature sets depending on your needs:

* **Production (Release, No Networking):**
  ```bash
  cargo run --release
  ```

* **Wi-Fi Version (Web Interface Enabled):**
  ```bash
  cargo run --release --features wifi
  ```

* **HIL Testing Version (Simulation Mode):**
  ```bash
  cargo run --release --features hil_test
  ```

* **Full Featured (Wi-Fi + HIL Testing):**
  ```bash
  cargo run --release --all-features
  ```

### 2. Run Python HIL Tests
If running the `hil_test` or full version:
```bash
cd tests
uv run python -m unittest test_oximite.py
```

## 🛠️ Production Optimizations
The `release` profile is configured for maximum performance and minimal binary size:
- `opt-level = 3`
- `lto = "fat"` (Link Time Optimization)
- `codegen-units = 1`
- `panic = "abort"`
- Symbols are stripped automatically.
