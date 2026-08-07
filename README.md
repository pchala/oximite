# oximite

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/J5E121FXSB)

**oximite** is a high-performance, asynchronous Rust firmware for the RP2350, designed to retrofit standard espresso machines (such as the Gaggia Classic or Rancilio Silvia) with advanced digital controls.

By replacing the factory internals with oximite, machines gain capabilities typically found in commercial or prosumer equipment, including real-time pressure profiling, PID temperature control, volumetric dosing, and a web-based user interface natively hosted on the Pico Plus 2 W.

Support the continued development of this open-source project by [leaving a tip on Ko-fi](https://ko-fi.com/J5E121FXSB). Contributions help accelerate the implementation of new features.

>Special thanks to [Pimoroni](https://pimoroni.com) for providing a [Pico Plus 2 W](https://shop.pimoroni.com/products/pimoroni-pico-plus-2-w) used in the project.

## Features

*   **PID Temperature Control:** Configurable Kp, Ki, and Kd parameters for both brewing and steaming. Controls the boiler via Solid State Relays (SSR) to maintain thermal stability.
*   **Pressure Profiling & Flow Control:** Advanced pump modulation via phase-angle triac firing, hardware-synced to 50/60Hz AC mains (zero-cross detection). Supports dynamic adjustment of target pressure and flow rate (ml/s).
*   **Volumetric Dosing:** Accurate dose measurement utilizing a Hall-effect flow meter.
*   **Live Web Dashboard:** An interactive web interface served directly over Wi-Fi. Features live telemetry graphing (pressure, flow, volume), machine controls (brew, flush, steam, descale), and an integrated settings manager.
*   **Custom Brew Profiles:** Create, save, and execute multi-step extraction profiles targeting time, volume, pressure, or flow. Up to 10 profiles can be persisted in the Pico's flash memory.
*   **Telemetry & Command Socket:** An open TCP command socket streams high-granularity telemetry and accepts remote commands. This enables automated testing out-of-the-box and serves as a backend for custom companion apps, high-resolution graphing tools, and unlimited profile sharing.

## Architecture

The firmware is written in Rust and utilizes the `embassy-rp` asynchronous framework to distribute workloads across the RP2350's dual cores:

*   **Core 0 (I/O & Control):** Handles hard-real-time I/O tasks. It runs PIO state machines for zero-cross detection, triac firing, flow meter pulse counting, hardware ADC sampling, and acts as the main state coordinator.
*   **Core 1 (Networking):** Dedicated to the CYW43 Wi-Fi driver, the TCP/IP networking stack, and serving the embedded HTTP Web Server.

## Pin Assignments

| Pin / GPIO | Peripheral / Name | Direction | Description |
| :--- | :--- | :--- | :--- |
| **GP0** | PIO0 (SM2) | Output | **Triac Control:** Phase-angle firing for pump/heater modulation. |
| **GP2** | Standard GPIO | Output | **Heater Relay:** Solid State Relay (SSR) control for the boiler. |
| **GP3** | Standard GPIO | Output | **3-Way Valve:** Solid State Relay (SSR) control for the brew group solenoid. |
| **GP5** | GPIO (Pull-Up) | Input | **Power Button:** Physical button to wake/sleep the machine. |
| **GP6** | GPIO (Pull-Up) | Input | **Brew Button:** Physical button to start/stop brewing. |
| **GP7** | GPIO (Pull-Up) | Input | **Steam Button:** Physical button to toggle steam mode. |
| **GP8** | GPIO (Pull-Up) | Input | **Flush Button:** Physical button for quick grouphead flush. |
| **GP9** | PIO0 (SM3) | Output | **WS2812 RGB LED:** Status indication via addressable LEDs. |
| **GP10** | PIO0 (SM1) | Input | **Zero-Cross:** Syncs Triac firing with 50/60Hz AC mains. |
| **GP15** | PIO0 (SM0) | Input | **Flow Meter:** Reads pulses from a Hall-effect water flow sensor. |
| **GP23** | WL_ON | Output | *Internal:* CYW43 Wi-Fi chip power control. |
| **GP24** | WL_D / PIO1 | In/Out | *Internal:* CYW43 Wi-Fi SPI Data. |
| **GP25** | WL_CS | Output | *Internal:* CYW43 Wi-Fi Chip Select. |
| **GP26** | GPIO (HiZ) | — | *Unused:* Held high-impedance; shares PCB net with A0 (GP40). |
| **GP27** | GPIO (HiZ) | — | *Unused:* Held high-impedance; shares PCB net with A1 (GP41). |
| **GP29** | WL_CLK / PIO1 | Output | *Internal:* CYW43 Wi-Fi SPI Clock. |
| **GP40** | ADC (A0) | Analog In | **Pressure Sensor:** Reads analog voltage from the pressure transducer. |
| **GP41** | ADC (A1) | Analog In | **Temp Sensor:** Reads analog voltage from the thermistor/thermocouple. |

## AC Actuator Timing

The pump has an internal diode, so its coil is only energised on one half-wave
of the mains — one stroke per full cycle. The control loop therefore runs at
50 Hz, locked to the mains rather than to a software timer.

The zero-cross input is an open-collector output: it is pulled LOW actively
(sharp edge) during positive AC wave and released HIGH through a pull-up (an RC ramp) during negative wave. The falling edge is therefore the only precise timing reference, and everything is referenced to it. `ZC LOW` is the conducting half-wave.

### Zero-cross detection

One ZC pin feeds three state machines, each clocked for its own job:

| SM | Clock | Counts | Role |
| :-- | :-- | :-- | :-- |
| trigger (SM1) | 2 MHz | 2 instr/loop = **1 µs** per count | measures the LOW window, paces the control loop |
| triac (SM2) | 1 MHz | 1 instr/loop = **1 µs** per count | fires the pump gate at an offset from the falling edge |
| heater (SM0) | 16 kHz | `nop [31]` = **2 ms** | latches the heater bit clear of the crossing |

**The trigger SM defines the control tick.** It aligns on `wait 1` then `wait 0`,
counts while the pin stays LOW, and pushes at the *rising* edge — one word per
full mains cycle, which is what makes the loop 50 Hz rather than 100 Hz. The
pushed value is the LOW window in µs, not the mains half-period; the two differ
by the opto's conduction angle.

That distinction does not matter, because the triac SM starts its own count from
the *same* falling edge and the LUT is expressed as a fraction of that same
window. Firing angle is therefore stated in units that cancel: `delay =
fraction(duty) x ac_ema`, with the LUT running 0.6 at 0 % down to 0.2 at 100 %.
Nothing in the chain assumes 50 Hz, so 60 Hz mains and cycle-to-cycle wander are
tracked for free.

The control task guards that measurement three ways: it drains the FIFO and
keeps only the newest word, so a late tick cannot act on a stale period; it
accepts only 7 500-11 500 µs, which brackets both 50 and 60 Hz while rejecting
noise-triggered edges; and it smooths what survives with an EMA (`alpha = 0.10`,
~10-cycle time constant). A 25 ms timeout on the pull keeps the loop spinning if
mains is absent, so the firmware still runs with no AC connected.

The heater SM waits for the falling edge, then idles a fixed 2 ms before
latching. That clears the race around the true crossing regardless of
transformer lag, so the bit is stable well before the zero-cross MOC acts on it.
Its `pull noblock` reads 0 when the FIFO is empty, and 0 means OFF — if the
control task ever stops feeding it, the heater fails safe in hardware without
needing a watchdog.

Four consecutive cycles, with `Dn` the pump duty and `Hn` the heater bit decided
on cycle *n*:

| t (ms) | ZC | Half-wave | Control task | Pump triac (random-phase MOC) | Piston / water | Heater (zero-cross MOC) |
| ---: | :--: | :-- | :-- | :-- | :-- | :-- |
| 0  | ↑ | HIGH | wake; sample ADC + flow; compute `D1`,`H1`; push both FIFOs | SM parked in `wait 0` holding `D1` | delivering the previous stroke | — |
| 10 | ↓ | **LOW** | idle | edge seen → delay `d(D1)` → gate at `10+d1` | **intake** — spring compressing, *flow sensor sees this* | `wait 0` → `nop [31]` |
| 20 | ↑ | HIGH | wake; compute `D2`,`H2` | mains current zero → triac commutates off | **delivery** of `D1` into the puck | `H1` latched at 12 ms; MOC turns on here for one full cycle |
| 30 | ↓ | **LOW** | idle | gate at `30+d2` | intake for `D2` | `H2` latched at 32 ms |
| 40 | ↑ | HIGH | wake; compute `D3`,`H3` | off | delivery of `D2` | `H2` conducts |
| 50 | ↓ | **LOW** | idle | gate at `50+d3` | intake for `D3` | `H3` latched at 52 ms |
| 60 | ↑ | HIGH | wake; compute `D4`,`H4` | off | delivery of `D3` | `H3` conducts |
| 70 | ↓ | **LOW** | idle | gate at `70+d4` | intake for `D4` | `H4` latched at 72 ms |
| 80 | ↑ | HIGH | wake; compute `D5`,`H5` | off | delivery of `D4` | `H4` conducts |

Consequences of this layout:

*   **A decision takes one full cycle to reach the water.** `Dn` is computed at
    `20n` and delivered at `20n+20`.
*   **The two triacs never switch together.** The pump uses a random-phase MOC
    and fires at its gate pulse mid-half-wave; the heater uses a zero-cross MOC
    which defers turn-on to the next true mains crossing. Switching transients
    from one cannot land on the other's measurement.
*   **`pull block` precedes the edge waits deliberately.** With the pump off the
    state machine parks *before* consuming an edge, so the first word pushed
    after a start syncs to a fresh cycle instead of firing at an arbitrary angle.

## Flow Signal

The flow meter is a Hall-effect turbine; the PIO times each HIGH and LOW phase
separately, giving two edges per magnet pass.

**The rotor is a mechanical low-pass filter, and it dominates the loop.** The
sensor sits on the pump *inlet*, and a positive-displacement pump blocks its own
inlet when it stops (the three-way valve also isolates the boiler downstream),
so once the pump stops there is no path for water to move at all — yet the
sensor keeps emitting edges. Across ten bench runs it coasted a median of
**0.18 ml over ~240 ms**, and identically whether it stopped at 0.18 bar or at
10.7 bar.


## Getting Started

### 1. Build & Flash

Ensure the Rust `thumbv8m.main-none-eabihf` target is installed.

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
