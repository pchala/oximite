# Oximite Architecture: States and Commands

## States

### `MachineState` (Enum - `src/state.rs`)
Represents the top-level operational state of the machine.
- `Idle`
- `Brewing`
- `Steaming`
- `Pumping`
- `Descaling`
- `Sleeping`
- `Cooling`

### `AdcState` (Struct - `src/control.rs`)
Represents the current telemetry and targets for the ADC / Control systems.
- `pressure_bar`: f32
- `temp_c`: f32
- `target_bar`: f32
- `target_temp`: f32
- `flow_limit_ml_s`: f32

### `FlowState` (Struct - `src/flow_meter.rs`)
Represents the current telemetry for the flow meter.
- `flow_rate_ml_s`: f32
- `total_volume_ml`: f32

## Commands & Events

### `MachineCommand` (Enum - `src/state.rs`)
Commands sent to the central coordinator task.
- `RunProfile(BrewProfile)`
- `Brew`
- `Stop`
- `Steam`
- `Flush`
- `Descale`
- `DirectPump(f32)`
- `CooldownFlush`
- `ProfileFinished`
- `SaveSettings(SettingsManager)`

### `HardwareCommand` (Enum - `src/control.rs`)
Commands sent specifically to the hardware execution task.
- `RunProfile(BrewProfile)`
- `Steam`
- `Descale`
- `DirectPump(f32)`
- `CooldownFlush`

### `SystemEvent` (Enum - `src/main.rs`)
Events related to system-level operations, like saving to flash.
- `SaveSettings(SettingsManager)`
- `SaveProfile(u8)`
- `DeleteProfile(u8)`

### `ApiCommand` (Struct - `src/wifi_task.rs`)
Payload structure for commands received via the web API.
- `cmd`: &str
- `profile`: Option<BrewProfile>
- `slot`: Option<u8>
- `machine`: Option<MachineSettings>
- `hardware`: Option<HardwareSettings>
- `temp_pid`: Option<PidSettings>
- `press_pid`: Option<PidSettings>
- `wifi`: Option<WifiSettings>
- `power`: Option<f32>

### `UartCommand` (Struct - `src/uart_task.rs`)
Payload structure for commands received via UART.
- `cmd`: &str
- `profile`: Option<BrewProfile>
- `settings`: Option<SettingsManager>
- `power`: Option<f32>