# Oximite System Messages and Signals

This document lists all internal messaging signals and state watches used for inter-task communication within the Oximite firmware.

## State & Command Channels

### `SIG_COMMAND` (Signal<MachineCommand>)
- **Type:** `Signal`
- **Purpose:** Central channel for sending high-level commands to the machine coordinator.
- **Senders:** `wifi_task`, `uart_task`, `buttons_task`, `hardware_task` (when profiles/actions finish).
- **Receivers:** `coordinator_task` (`main.rs`).

### `MACHINE_STATE` (Watch<MachineState>)
- **Type:** `Watch`
- **Purpose:** Broadcasts the current high-level state of the machine.
- **Senders:** `state::set_state()`.
- **Receivers:** `led_update_task` (`main.rs`). (Note: State is also accessible synchronously via `get_state()`).

## System Events

### `SIG_SYSTEM_EVENT` (Signal<SystemEvent>)
- **Type:** `Signal`
- **Purpose:** Queues background system events, primarily saving to flash memory.
- **Senders:** `wifi_task`, `coordinator_task`, `hardware_task`.
- **Receivers:** `system_events_task` (`main.rs`).

### `SIG_WIFI_RECONFIG` (Signal<()>)
- **Type:** `Signal`
- **Purpose:** Triggers a restart of the Wi-Fi stack when credentials are updated.
- **Senders:** `coordinator_task` (`main.rs`).
- **Receivers:** `wifi_task`.

## Flow Meter Telemetry

### `FLOW_WATCH` (Watch<FlowState>)
- **Type:** `Watch`
- **Purpose:** Broadcasts real-time flow rate and total volume metrics.
- **Senders:** `run_flow_task` (`flow_meter.rs`).
- **Receivers:** Read via `FlowMonitor::get_state()` by `led_update_task`, `pump_control_task`, `execute_profile`, `hardware_task`, `wifi_task`, `uart_task`.

### `SIG_RESET_VOLUME` (Signal<()>)
- **Type:** `Signal`
- **Purpose:** Requests the flow meter task to reset its total accumulated volume.
- **Senders:** `FlowMonitor::reset_volume()`.
- **Receivers:** `run_flow_task` (`flow_meter.rs`).

### `SIG_RESET_ACK` (Signal<()>)
- **Type:** `Signal`
- **Purpose:** Acknowledges that the flow volume has been successfully reset.
- **Senders:** `run_flow_task` (`flow_meter.rs`).
- **Receivers:** `FlowMonitor::reset_volume()` (awaits acknowledgment).

## Hardware & Control Signals

### `SIG_HARDWARE_CMD` (Signal<HardwareCommand>)
- **Type:** `Signal`
- **Purpose:** Commands the hardware executor task to run profiles, steam, descale, or direct pump.
- **Senders:** `coordinator_task` (`main.rs`).
- **Receivers:** `hardware_task` (`main.rs`).

### `SIG_PROFILE_ABORT` (Signal<()>)
- **Type:** `Signal`
- **Purpose:** Cancels any currently executing hardware routine (profiles, steaming, etc.).
- **Senders:** `coordinator_task` (`main.rs`).
- **Receivers:** `hardware_task` (`main.rs`).

### `ADC_WATCH` (Watch<AdcState>)
- **Type:** `Watch`
- **Purpose:** Broadcasts the latest filtered ADC telemetry (pressure, temperature) along with current PID targets.
- **Senders:** `adc_task`, `pump_control_task` (updates targets), `heater_control_task` (updates targets).
- **Receivers:** Read via `AdcMonitor::get_state()` by `led_update_task`, `pump_control_task`, `heater_control_task`, `execute_steam`, `wifi_task`, `uart_task`.

### `SIG_TARGET_PRESSURE` (Signal<f32>)
- **Type:** `Signal`
- **Purpose:** Updates the target pressure setpoint for the pump PID controller.
- **Senders:** `control::set_target_pressure()`.
- **Receivers:** `pump_control_task` (`control.rs`).

### `SIG_FLOW_LIMIT` (Signal<f32>)
- **Type:** `Signal`
- **Purpose:** Updates the flow limit setpoint for the pump PID controller.
- **Senders:** `control::set_flow_limit()`.
- **Receivers:** `pump_control_task` (`control.rs`).

### `SIG_TARGET_TEMP` (Signal<f32>)
- **Type:** `Signal`
- **Purpose:** Updates the target temperature setpoint for the heater PID controller.
- **Senders:** `control::set_target_temp()`.
- **Receivers:** `heater_control_task` (`control.rs`).

### `SIG_DIRECT_PUMP` (Signal<Option<f32>>)
- **Type:** `Signal`
- **Purpose:** Directly sets the pump power percentage, overriding the PID controller.
- **Senders:** `control::set_direct_pump()`.
- **Receivers:** `pump_control_task` (`control.rs`).