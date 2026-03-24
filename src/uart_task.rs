use core::str::from_utf8;
use embassy_rp::uart::{Async, UartRx, UartTx};
use embassy_time::{Duration, Ticker, Timer};
use serde::{Deserialize, Serialize};

use crate::settings::BrewProfile;
use crate::state::{get_state, MachineCommand, SIG_COMMAND};
use crate::{control, flow_meter};

#[derive(Serialize)]
struct Telemetry<'a> {
    st: u8,
    p: f32,
    tp: f32,
    t: f32,
    tt: f32,
    fl: f32,
    vol: f32,
    msg: &'a str,
}

#[derive(Deserialize)]
struct UartCommand<'a> {
    cmd: &'a str,
    profile: Option<BrewProfile>,
    settings: Option<crate::settings::SettingsManager>,
    power: Option<f32>,
}

#[embassy_executor::task]
pub async fn uart_tx_task(mut tx: UartTx<'static, Async>) {
    let mut ticker = Ticker::every(Duration::from_millis(20));

    loop {
        let a = control::AdcMonitor::new().get_state().await;
        let f = flow_meter::FlowMonitor::new().get_state().await;
        let current_state = get_state() as u8;

        let data = Telemetry {
            st: current_state,
            p: a.pressure_bar,
            tp: a.target_bar,
            t: a.temp_c,
            tt: a.target_temp,
            fl: f.flow_rate_ml_s,
            vol: f.total_volume_ml,
            msg: "ok",
        };

        if let Ok(mut json_str) = serde_json_core::to_string::<_, 256>(&data) {
            let _ = json_str.push_str("\n");
            let _ = tx.write(json_str.as_bytes()).await;
        }

        ticker.next().await;
    }
}

#[embassy_executor::task]
pub async fn uart_rx_task(mut rx: UartRx<'static, Async>) {
    let mut line_buf = [0u8; 1024];
    let mut line_pos = 0;

    loop {
        let mut byte = [0u8; 1];
        // Read 1 byte at a time to ensure we catch the newline immediately.
        // Even at 2Mbit/s, the RP2040 can handle this if the loop is tight.
        match rx.read(&mut byte).await {
            Ok(_) => {
                let b = byte[0];
                if b == b'\n' {
                    if line_pos > 0 {
                        if let Ok(json_str) = from_utf8(&line_buf[..line_pos]) {
                            handle_command(json_str);
                        }
                        line_pos = 0;
                    }
                } else if b != 0 && line_pos < line_buf.len() {
                    line_buf[line_pos] = b;
                    line_pos += 1;
                }
            }
            Err(_) => {
                Timer::after(Duration::from_millis(1)).await;
            }
        }
    }
}

fn handle_command(json_str: &str) {
    match serde_json_core::from_str::<UartCommand>(json_str) {
        Ok((payload, _)) => match payload.cmd {
            "stop" => SIG_COMMAND.signal(MachineCommand::Stop),
            "brew" => SIG_COMMAND.signal(MachineCommand::Brew),
            "steam" => SIG_COMMAND.signal(MachineCommand::Steam),
            "flush" => SIG_COMMAND.signal(MachineCommand::Flush),
            "descale" => SIG_COMMAND.signal(MachineCommand::Descale),
            "direct_pump" => {
                SIG_COMMAND.signal(MachineCommand::Stop);
                if let Some(p) = payload.power {
                    SIG_COMMAND.signal(MachineCommand::DirectPump(p));
                }
            }
            "profile" => {
                if let Some(p) = payload.profile {
                    defmt::info!("UART: Running profile '{}'", p.name.as_str());
                    SIG_COMMAND.signal(MachineCommand::RunProfile(p));
                }
            }
            "save_settings" => {
                if let Some(s) = payload.settings {
                    defmt::info!("UART: Saving settings");
                    SIG_COMMAND.signal(MachineCommand::SaveSettings(s));
                }
            }
            _ => {
                defmt::warn!("Unknown UART command: {}", payload.cmd);
            }
        },
        Err(e) => {
            defmt::error!(
                "UART JSON Parse Error: {:?} for string: {}",
                defmt::Debug2Format(&e),
                json_str
            );
        }
    }
}
