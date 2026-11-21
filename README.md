# oximite

**oximite** is an asynchronous Rust firmware suite for the Pimoroni Pico Plus 2 W (RP2350B). It retrofits domestic espresso machines (such as the Gaggia Classic) with digital temperature control (with flow feed-forward), unified pressure/flow pump profiling, a lightweight offline web UI on port 80, and a native desktop companion application (`control/`) over TCP port 8080.

Special thanks to [Pimoroni](https://pimoroni.com) for providing hardware.

Support development by [leaving a tip on Ko-fi](https://ko-fi.com/J5E121FXSB).

[![ko-fi](https://ko-fi.com/img/githubbutton_sm.svg)](https://ko-fi.com/J5E121FXSB)

## Workspace layout

- `proto/`: Shared `no_std` domain models and Port 80/8080 wire codecs (`oximite-proto`).
- `firmware/`: RP2350B firmware and lightweight offline HTML dashboard.
- `host-tests/`: Host unit, property/fuzz, and closed-loop simulation test suite.
- `control/`: Native Rust desktop application (`oximite-control`) for live graphing, PID tuning, profile editing, and bench sweeps.

## Getting started

- Repository layout, build, and test commands: [AGENTS.md](AGENTS.md).
- Documentation: [docs/](docs/). Start at [Requirements](docs/requirements.md).
- First-time setup: [Operations](docs/operations.md).

## Disclaimer

All hardware designs, code, and documentation are provided "AS IS", without warranty of any kind.
You are solely responsible for electrical and thermal safety when working with AC mains voltages and pressurized steam boilers.
