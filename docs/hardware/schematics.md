# Electrical schematics and circuit interfaces

This document describes the circuit topology connecting the 3.3 V microcontroller to AC mains actuators and sensors. Complete KiCad 10 schematic files, hierarchical sheets (`AC_Mains.kicad_sch`, `Control.kicad_sch`), and board layout are located in `pcb/`.

## Zero-cross detector circuit

The zero-cross circuit isolates mains voltage and produces a clean digital pulse at each half-cycle:

- **AC side:** AC Line and Neutral connect through two 47 kΩ 1W resistors in series to the input of a bidirectional optocoupler (PC814 or EL814).
- **DC side:** The phototransistor collector connects to GP10 with a 10 kΩ pull-up to 3.3 V. The emitter connects to GND.
- **Waveform:** GP10 pulls LOW during the conduction window of the AC wave and releases HIGH as the voltage crosses zero.

```
AC L ---[ 47k 1W ]---+---|>|--+
                     |        |  (PC814) ===> Collector --- GP10 (3.3V pull-up)
AC N ---[ 47k 1W ]---+---|<|--+               Emitter   --- GND
```

## Pump triac driver

The Ulka/Ceme vibration pump inductive load is modulated via a phase-angle triac:

- **Optoisolator:** MOC3021 (random-phase optotriac). A random-phase driver is required to trigger conduction at arbitrary points within the half-cycle.
- **Power Triac:** BTA08-600B (snubberless or with external RC snubber).
- **Snubber:** 39 Ω 1W resistor in series with a 10 nF 630V capacitor across triac MT1 and MT2 to suppress inductive kickback.

## Boiler heater SSR

Boiler thermal power uses a dedicated Solid State Relay (SSR):

- Driven directly from GP2 (active HIGH).
- Uses a zero-cross optocoupler (e.g., MOC3041 or commercial SSR-25DA) to minimize EMI. Switching occurs only at the true zero crossing.

## Analog input conditioning

- **Pressure transducer (GP40 / A0):** 3.3V-powered piezoresistive transducer (0–1.2 MPa, G1/8") outputting 0.4 V at 0 bar to 2.4 V at 12 bar (6 bar/V slope). Connects directly to the RP2350 GP40 ADC input without a voltage divider.
- **Thermistor (GP41 / A1):** Semitec 104NT-4-R025H42G NTC thermistor (100 kΩ @ 25 °C, B25/85 = 4267 K) connected between GP41 and GND, with a precision **10 kΩ 0.1% pull-up resistor** to 3.3 V. Linearized via the 1025-entry lookup table in `adc.rs`.
