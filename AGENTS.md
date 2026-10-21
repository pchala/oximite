# AGENTS.md

Instructions for AI coding agents and new contributors.

## Repository

- Platform: Pimoroni Pico Plus 2 W (RP2350B), Rust `no_std` + Embassy in `firmware/`.
- Dual-core operation: Core 0 owns real-time control. Core 1 owns networking.


| Path | Content |
|---|---|
| `README.md` | Repository map and documentation entry point |
| `docs/` | System, architecture, interfaces, and governance documents |
| `firmware/` | Rust `no_std` + Embassy firmware crate (`src/`, `Cargo.toml`, `build.rs`) |
| `firmware/html/` | Embedded web dashboard served over HTTP |
| `firmware/cyw43-firmware/` | Binary Wi-Fi blobs from third party. Do not edit |
| `pcb/` | KiCad schematic, board layout, project symbols, and footprints |
| `mech/` | Mechanical brackets and 3D models (OpenSCAD) |
| `tests/` | Hardware-in-the-loop and network test suite |

## 2. Validate a change

Run all steps before you report a firmware change as done:

```powershell
cd firmware
cargo check
cargo clippy
cargo fmt
```

`cargo clippy` MUST report zero warnings.

Hardware-in-the-loop and network tests (when connected to a target or test rig):

```powershell
cd tests
uv run python -m unittest test_oximite.py
```

Then update the documentation files that own the changed facts (section 4).

## 3. Firmware rules (do not break)

**Architecture and core roles**
- **Core 0 role:** Hard real-time control only. Runs zero-cross synchronization (PIO0 SM1), pump triac firing (PIO0 SM2), heater modulation (PIO0 SM0), flow meter counting (PIO2 SM0), ADC sampling, PID loops, state coordinator, and flash persistence.
- **Core 1 role:** Networking and telemetry only. Runs the CYW43 Wi-Fi driver, TCP/IP stack (`embassy-net`), HTTP server (port 80), and postcard telemetry/command socket (port 8080).
- **Inter-core communication:** Data MUST cross cores only through lock-free atomics and channels in `state.rs`. Never use blocking mutexes across cores.
- **No heap:** Use static allocations, `heapless` collections, and `static_cell`.

**AC actuators and timing**
- **Mains synchronization:** The control loop runs at mains frequency (50/60 Hz), synchronized to the falling edge of the zero-cross input.
- **Triac vs heater MOC:** The pump triac uses a random-phase MOC and fires mid-half-wave through a delay lookup table. The boiler heater uses a zero-cross MOC latched 2 ms after the falling edge. They MUST NOT switch simultaneously.
- **Heater fail-safe:** The heater PIO state machine uses `pull noblock`. An empty FIFO reads 0 (heater OFF). If the control loop stalls, the heater shuts off automatically in hardware without a watchdog requirement.

**Dual-PID control loops**
- **Temperature PID (`temp_pid`):** Regulates boiler temperature at 5 Hz (every 10 ticks at 50 Hz). During active extraction, it applies real-time flow feed-forward (`target_t + flow_ml_s * feed_forward`) to offset thermal drop from incoming cold water before boiler metal cools.
- **Pressure and flow PID (`pump_pid`):** Modulates the vibration pump triac at 50 Hz (every mains half-wave). Uses a unified dimensionless controller where pressure and flow errors are normalized to 1.0; the binding constraint (`max(u_p, u_q)`) governs pump power.

**Hydraulics and sensors**
- **Flow meter:** Digmesa Nano DM60 turbine (49,162 pulses/L, 98,324 edges/L timed via PIO2 SM0). Direct pulse accumulation to milliliter volume.
- **ADC inputs:** GP26 and GP27 share analog nets with GP40 (A0) and GP41 (A1). Keep GP26 and GP27 in high-impedance mode (`Pull::None`) at all times.

**Safe-boot and network**
- **Safe-boot AP mode:** If GP8 (Right Toggle Switch UP / Flush) is held during boot, the firmware MUST force Wi-Fi AP mode to allow recovery of settings.
- **Dual telemetry paths:** Port 80 serves lightweight JSON for web browsers. Port 8080 streams raw 55-byte postcard frames. Keep these interfaces independent.

## 4. Documentation rules

- Put all documentation in `docs/`. Only `README.md` and `AGENTS.md` reside at the repository root.
- Use `docs/firmware.md` for firmware design, and `docs/hardware/` and `docs/research/` for hardware and research details.
- Put system-wide requirements, architecture, operations, and shared contracts directly in `docs/`.
- Each fact has one home document. Link to it. Do not duplicate it.
- Record every architecture and design decision in `docs/decisions.md`.
- Record unresolved questions in `docs/open-questions.md`. Do not keep question lists in subsystem documents.
- Draw diagrams in Mermaid, not ASCII art.
- Write in ASD-STE100 Simplified Technical English: short sentences, active voice, one instruction per sentence.
- Use uppercase RFC 2119 keywords (`MUST`, `SHOULD`, `MAY`) in requirements.
