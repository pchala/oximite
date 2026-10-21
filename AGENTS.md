# AGENTS.md

Instructions for AI coding agents and contributors. Global agent rules (workflow, code style, git) apply in addition to this file. This file only adds project-specific rules.

## Project

oximite is Rust `no_std` + Embassy firmware for the Pimoroni Pico Plus 2 W (RP2350B). It retrofits domestic espresso machines with digital temperature, pressure, and flow control.

- `docs/`: All project documentation. Start at [Requirements](docs/requirements.md).
- `firmware/`: Firmware crate (`src/`, `Cargo.toml`, `build.rs`).
- `firmware/html/`: Embedded web dashboard served over HTTP.
- `firmware/cyw43-firmware/`: Third-party Wi-Fi blobs. Do not edit.
- `pcb/`: KiCad schematic, board layout, symbols, and footprints.
- `mech/`: OpenSCAD models for mounting brackets.
- `tests/`: Hardware-in-the-loop and network test suite.

## Build and validate

Install the Rust `thumbv8m.main-none-eabihf` target first.

Run all steps before you report a firmware change as done:

```powershell
cd firmware
cargo fmt
cargo check
cargo clippy
```

`cargo clippy` MUST report zero warnings.

When a target or test rig is connected, run the hardware-in-the-loop and network tests:

```powershell
cd tests
uv run python -m unittest test_oximite.py
```

Say clearly in your report if you could not run the hardware tests.

## Safety

The firmware switches AC mains loads and heats a pressurized boiler. Treat every change to heater, pump, or sensor code as safety-relevant.

- Do not weaken or remove a safety limit, fail-safe, or timeout. Safety limits are defined in [Requirements](docs/requirements.md).
- Do not change actuator timing without reading [Firmware design](docs/firmware.md) section 4.

## Firmware rules (do not break)

Each rule links to the document that owns the details.

- **Core split:** Core 0 runs hard real-time control only. Core 1 runs networking and telemetry only. See [Firmware design](docs/firmware.md) section 2.
- **Inter-core data:** Data MUST cross cores only through the lock-free atomics and channels in `state.rs`. Never use blocking mutexes across cores.
- **No heap:** Use static allocations, `heapless` collections, and `static_cell`.
- **Mains synchronization:** The control loop MUST stay locked to the zero-cross falling edge.
- **Actuator isolation:** The pump triac and the heater MUST NOT switch at the same time.
- **Heater fail-safe:** The heater state machine MUST keep `pull noblock`. An empty FIFO MUST turn the heater off.
- **Shared analog nets:** GP26 and GP27 MUST stay high-impedance. See [Hardware pinout](docs/hardware/pinout.md).
- **Safe boot:** The safe-boot Access Point path (REQ-12) MUST always work.
- **Network interfaces:** Keep the HTTP interface (port 80) and the telemetry socket (port 8080) independent. See [Web interface](docs/web-interface.md) and [Telemetry socket](docs/telemetry-socket.md).

## Documentation rules

- Put all documentation in `docs/`. Only `README.md` and `AGENTS.md` stay at the repository root.
- Each fact has one home document. Link to it. Do not copy it.
- Update the home document in the same change as the code.
- Code owns values that it defines, such as frame sizes and lookup tables. Docs describe the purpose and point to the code.
- Record every architecture and design decision in [Decisions](docs/decisions.md). Do not delete or renumber rows.
- Record unresolved questions in [Open questions](docs/open-questions.md) only.
- Requirement IDs MUST NOT change or be reused.
- Draw diagrams in Mermaid, not ASCII art.
- Write in ASD-STE100 Simplified Technical English: short sentences, active voice, one instruction per sentence.
- Use uppercase RFC 2119 keywords (`MUST`, `SHOULD`, `MAY`) in requirements.

### Document homes

- [docs/requirements.md](docs/requirements.md): System requirements and safety limits.
- [docs/firmware.md](docs/firmware.md): Firmware architecture, control loops, timing, storage, machine states.
- [docs/operations.md](docs/operations.md): Modes, LEDs, provisioning, tuning.
- [docs/web-interface.md](docs/web-interface.md): HTTP API.
- [docs/telemetry-socket.md](docs/telemetry-socket.md): Port 8080 protocol.
- [docs/hardware/pinout.md](docs/hardware/pinout.md): GPIO, switches, shared nets.
- [docs/hardware/schematics.md](docs/hardware/schematics.md): Circuits and sensor conditioning.
- [docs/hardware/bom.md](docs/hardware/bom.md): Components and ratings.
- [docs/hardware/mechanics.md](docs/hardware/mechanics.md): Mounting and hydraulics.
- [docs/decisions.md](docs/decisions.md): Decisions.
- [docs/open-questions.md](docs/open-questions.md): Open questions.
