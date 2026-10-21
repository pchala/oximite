# oximite

**oximite** is an asynchronous Rust firmware for the RP2350 (Pimoroni Pico Plus 2 W), designed to retrofit standard domestic espresso machines (such as the Gaggia Classic) with advanced digital controls: dual PID loops (boiler temperature with real-time flow feed-forward, and unified pressure/flow pump profiling), and an embedded web dashboard.

Special thanks to [Pimoroni](https://pimoroni.com) for providing hardware.

Support development by [leaving a tip on Ko-fi](https://ko-fi.com/J5E121FXSB). 

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/J5E121FXSB)

## Repository

| Path                       | Content                                                                              |
| -------------------------- | ------------------------------------------------------------------------------------ |
| `docs/`                    | Documentation                                                                        |
| `firmware/`                | Rust `no_std` + Embassy firmware crate for RP2350 (`src/`, `Cargo.toml`, `build.rs`) |
| `firmware/html/`           | Embedded web interface assets                                                        |
| `firmware/cyw43-firmware/` | Binary Wi-Fi blobs from third party. Do not edit                                     |
| `pcb/`                     | KiCad schematic, PCB layout, project symbols, and footprints                         |
| `mech/`                    | OpenSCAD 3D models for mounting brackets                                             |
| `tests/`                   | Automated hardware-in-the-loop and network test suite                                |

## Getting Started

### Build & Check

Ensure the Rust `thumbv8m.main-none-eabihf` target is installed.

```powershell
cd firmware
cargo check
cargo clippy
```

### Automated Tests

Run the integration test suite:

```powershell
cd tests
uv run python -m unittest test_oximite.py
```

## Disclaimer

All hardware designs, code, and documentation are provided "AS IS", without warranty of any kind.
You are solely responsible for electrical and thermal safety when working with AC mains voltages and pressurized steam boilers.
