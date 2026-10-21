# System requirements

## Purpose

Oximite retrofits domestic espresso machines with digital temperature, pressure, flow profiling, and volumetric dosing controls.

Specific subsystem designs are in [Firmware design](firmware.md) and [Hardware pinout](hardware/pinout.md).

## Requirements

This document lists active system requirements. Requirement IDs MUST NOT change or be reused.

### Thermal control

- **REQ-01** The system MUST control boiler temperature using a dedicated closed PID loop. During active extraction, it MUST apply real-time flow feed-forward to compensate incoming water.
- **REQ-02** The system MUST maintain separate target setpoints and PID tuning parameters for brew mode and steam mode.
- **REQ-03** The system MUST shut down the heater immediately if boiler temperature exceeds the safety threshold of 145 °C or if the sensor is disconnected.  
  *Rationale:* Protect boiler gaskets and heating elements against thermal runaway.
- **REQ-14** The system MUST turn off the heater automatically if the heater stays on continuously for more than 3 min.  
  *Rationale:* Limit thermal runaway when a sensor or control fault occurs.
- **REQ-15** The steam mode target temperature MUST be configurable. The system MUST limit it to a safety ceiling of 145 °C.

### Pressure and flow control

- **REQ-04** The system MUST modulate vibration pump pressure using AC phase-angle firing synchronized to AC mains zero crossings.
- **REQ-05** The system MUST support two-dimensional profile steps with simultaneous pressure (bar) and flow rate (ml/s) targets.
- **REQ-06** The system MUST measure water volume using a Hall-effect flow turbine on the pump inlet.
- **REQ-13** Profile steps MUST support dual exit conditions evaluated against elapsed time and dispensed volume.
- **REQ-16** Flash storage MUST hold a maximum of 10 custom multi-step brew profiles.

### User interaction and telemetry

- **REQ-08** The system MUST provide an embedded web dashboard served over Wi-Fi on port 80.
- **REQ-09** The system MUST provide a streaming binary telemetry socket on port 8080 with postcard-encoded frames.
- **REQ-10** The system MUST accept JSON commands over TCP port 8080.
- **REQ-12** If the flush input (Right Toggle UP, GP8) is held during power-on, the system MUST start in Wi-Fi Access Point mode.  
  *Rationale:* Recover machine access when network credentials change or fail.

## Scope

The baseline includes:
- RP2350 microcontroller and CYW43 Wi-Fi subsystem.
- AC zero-cross detection, triac pump drive, and SSR boiler drive.
- Temperature and pressure and flow sensing.
- Embedded web dashboard and postcard telemetry socket.

