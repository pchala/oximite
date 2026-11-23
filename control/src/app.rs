//! VS Code dark-themed native GUI for Oximite control, graphing, and PID tuning.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use egui::{Color32, FontId, RichText, ScrollArea, Stroke, TextWrapMode};
use oximite_proto::model::{
    default_profile, BrewProfile, BrewProfileStep, MachineSettings, MachineState, PidSettings,
    PublicSettings, DEFAULT_SETTINGS, MAX_PROFILES, MAX_STEPS,
};

use crate::client::ClientHandle;
use crate::plot::{render_comparison_panels, render_live_3panel, PlotViewport};
use crate::store::{
    import_samples_jsonl, ControlStore, EventLogEntry, LoadedTrace, LogTag, ShotRecord,
    TelemetrySample,
};
use crate::sweeps::{SweepConfig, SweepResult, SweepRunner};

// VS Code Dark Theme Palette (matching `lite/control`)
const COLOR_SIDEBAR_BG: Color32 = Color32::from_rgb(37, 37, 38);
const COLOR_MAIN_BG: Color32 = Color32::from_rgb(30, 30, 30);
const COLOR_STATUSBAR_BG: Color32 = Color32::from_rgb(0, 122, 204);
const COLOR_CARD_BG: Color32 = Color32::from_rgb(45, 45, 48);
const COLOR_BORDER: Color32 = Color32::from_rgb(60, 60, 60);

const COLOR_HEALTH: Color32 = Color32::from_rgb(79, 193, 255);
const COLOR_PULSE: Color32 = Color32::from_rgb(78, 201, 176);
const COLOR_CMD: Color32 = Color32::from_rgb(220, 220, 170);
const COLOR_ACK: Color32 = Color32::from_rgb(197, 134, 192);
const COLOR_WARN: Color32 = Color32::from_rgb(244, 71, 71);
const COLOR_TEXT: Color32 = Color32::from_rgb(212, 212, 212);
const COLOR_DIM_TEXT: Color32 = Color32::from_rgb(140, 140, 140);
const COLOR_GREEN: Color32 = Color32::from_rgb(106, 153, 85);

const MAIN_PANEL_MARGIN: egui::Margin = egui::Margin::symmetric(12, 8);
const LOG_FONT_SIZE: f32 = 11.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppTab {
    LiveGraphs,
    Tuning,
    ProfilesSweeps,
    ReplayCompare,
    EventLog,
}

#[derive(Clone, PartialEq)]
struct LogViewKey {
    log_seq: u64,
    tag: Option<LogTag>,
    query: String,
    newest_first: bool,
}

#[derive(Default)]
struct LogView {
    key: Option<LogViewKey>,
    rows: Vec<EventLogEntry>,
}

struct Snapshot {
    connected: bool,
    connecting: bool,
    last_error: Option<String>,
    state: MachineState,
    sbt: f32,
    latest: Option<TelemetrySample>,
    settings: Option<PublicSettings>,
    profiles: Vec<oximite_proto::model::ProfileHeaderOwned>,
    loaded_profile: Option<(u8, BrewProfile)>,
    samples: Vec<TelemetrySample>,
    brew_samples: Vec<TelemetrySample>,
    shots: Vec<ShotRecord>,
    rx_hz: f32,
    dropped_frames: u64,
    ping_rtt: Option<f32>,
}

pub struct OximiteControlApp {
    store: Arc<Mutex<ControlStore>>,
    client: ClientHandle,
    start_time: Instant,
    active_tab: AppTab,
    endpoint_input: String,

    // Live graph state
    viewport: PlotViewport,
    brew_viewport: PlotViewport,
    cmp_viewport: PlotViewport,
    show_stats: bool,
    live_rect: egui::Rect,
    live_ctx_ppp: f32,
    image_rx: std::sync::mpsc::Receiver<Arc<egui::ColorImage>>,
    image_tx: std::sync::mpsc::Sender<Arc<egui::ColorImage>>,
    direct_pump_pct: f32,

    // Tuning drafts
    draft_machine: MachineSettings,
    draft_temp_pid: PidSettings,
    draft_pump_pid: PidSettings,
    draft_wifi_ssid: String,
    draft_wifi_pass: String,
    tuning_initialized: bool,
    tuning_status: Option<String>,

    // Profile editor
    prof_slot: u8,
    prof_name: String,
    prof_steps: Vec<BrewProfileStep>,
    last_loaded_profile_name: Option<String>,

    // Bench sweeps
    sweep_runner: Option<SweepRunner>,
    sweep_temp_base: f32,
    sweep_temp_target: f32,
    sweep_temp_cool_s: u64,
    sweep_temp_rec_s: u64,
    sweep_press_low: f32,
    sweep_press_high: f32,
    sweep_pump_step_pct: u8,
    sweep_pump_hold_s: f32,
    sweep_stability_s: u64,

    // Replay comparison
    trace_a: Option<LoadedTrace>,
    trace_b: Option<LoadedTrace>,
    replay_status: Option<String>,

    // Event log
    log_filter: Option<LogTag>,
    log_search: String,
    log_newest_first: bool,
    log_show_raw: bool,
    log_view: LogView,
}

pub fn apply_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    visuals.panel_fill = COLOR_MAIN_BG;
    visuals.window_fill = COLOR_MAIN_BG;
    visuals.faint_bg_color = COLOR_SIDEBAR_BG;
    visuals.extreme_bg_color = Color32::from_rgb(20, 20, 20);
    visuals.override_text_color = Some(COLOR_TEXT);
    ctx.set_visuals(visuals);
}

impl OximiteControlApp {
    pub fn new(store: Arc<Mutex<ControlStore>>, client: ClientHandle) -> Self {
        let endpoint = store
            .lock()
            .map(|s| s.endpoint.clone())
            .unwrap_or_else(|_| "192.168.1.117:8080".to_string());
        let def_prof = default_profile();
        if let Ok(mut st) = store.lock() {
            st.autosave_dir = Some(std::path::PathBuf::from("shots"));
        }
        let (image_tx, image_rx) = std::sync::mpsc::channel();
        Self {
            store,
            client,
            start_time: Instant::now(),
            active_tab: AppTab::LiveGraphs,
            endpoint_input: endpoint,
            viewport: PlotViewport::default(),
            brew_viewport: PlotViewport {
                window_secs: 0.0,
                ..Default::default()
            },
            live_rect: egui::Rect::NOTHING,
            live_ctx_ppp: 1.0,
            image_rx,
            image_tx,
            cmp_viewport: PlotViewport {
                auto_follow: false,
                window_secs: 0.0,
                ..Default::default()
            },
            show_stats: true,
            direct_pump_pct: 80.0,
            draft_machine: DEFAULT_SETTINGS.machine,
            draft_temp_pid: DEFAULT_SETTINGS.temp_pid,
            draft_pump_pid: DEFAULT_SETTINGS.pump_pid,
            draft_wifi_ssid: String::new(),
            draft_wifi_pass: String::new(),
            tuning_initialized: false,
            tuning_status: None,
            prof_slot: 0,
            prof_name: def_prof.name.to_string(),
            prof_steps: def_prof.steps.to_vec(),
            last_loaded_profile_name: None,
            sweep_runner: None,
            sweep_temp_base: 80.0,
            sweep_temp_target: 90.0,
            sweep_temp_cool_s: 15,
            sweep_temp_rec_s: 120,
            sweep_press_low: 2.0,
            sweep_press_high: 9.0,
            sweep_pump_step_pct: 5,
            sweep_pump_hold_s: 3.0,
            sweep_stability_s: 60,
            trace_a: None,
            trace_b: None,
            replay_status: None,
            log_filter: None,
            log_search: String::new(),
            log_newest_first: true,
            log_show_raw: false,
            log_view: LogView::default(),
        }
    }

    fn take_snapshot(&mut self) -> Snapshot {
        let Ok(st) = self.store.lock() else {
            return Snapshot {
                connected: false,
                connecting: false,
                last_error: Some("Store lock error".into()),
                state: MachineState::Idle,
                sbt: 92.0,
                latest: None,
                settings: None,
                profiles: Vec::new(),
                loaded_profile: None,
                samples: Vec::new(),
                brew_samples: Vec::new(),
                shots: Vec::new(),
                rx_hz: 0.0,
                dropped_frames: 0,
                ping_rtt: None,
            };
        };

        let key = LogViewKey {
            log_seq: st.log_seq,
            tag: self.log_filter,
            query: self.log_search.to_lowercase(),
            newest_first: self.log_newest_first,
        };
        if self.log_view.key.as_ref() != Some(&key) {
            let mut rows: Vec<EventLogEntry> = st
                .logs
                .iter()
                .rev()
                .filter(|e| key.tag.is_none_or(|t| e.tag == t))
                .filter(|e| {
                    key.query.is_empty()
                        || e.summary.to_lowercase().contains(&key.query)
                        || e.raw.to_lowercase().contains(&key.query)
                })
                .cloned()
                .collect();
            if !key.newest_first {
                rows.reverse();
            }
            self.log_view.rows = rows;
            self.log_view.key = Some(key);
        }

        Snapshot {
            connected: st.connected,
            connecting: st.connecting,
            last_error: st.last_error.clone(),
            state: st.current_state,
            sbt: st.session_brew_temp,
            latest: st.latest.clone(),
            settings: st.settings.clone(),
            profiles: st.profiles.clone(),
            loaded_profile: st.loaded_profile.clone(),
            samples: st.samples.iter().cloned().collect(),
            brew_samples: st.brew_samples(),
            shots: st.shots.clone(),
            rx_hz: st.rx_hz,
            dropped_frames: st.dropped_frames,
            ping_rtt: st.last_ping_rtt_ms,
        }
    }

    fn sync_drafts_if_needed(&mut self, snap: &Snapshot) {
        if !self.tuning_initialized {
            if let Some(ref s) = snap.settings {
                self.draft_machine = s.machine;
                self.draft_temp_pid = s.temp_pid;
                self.draft_pump_pid = s.pump_pid;
                self.draft_wifi_ssid = s.wifi.ssid.to_string();
                self.tuning_initialized = true;
            }
        }
        if let Some((slot, ref p)) = snap.loaded_profile {
            let key = format!("{slot}:{}", p.name.as_str());
            if self.last_loaded_profile_name.as_deref() != Some(key.as_str()) {
                self.prof_slot = slot;
                self.prof_name = p.name.to_string();
                self.prof_steps = p.steps.to_vec();
                self.last_loaded_profile_name = Some(key);
            }
        }
    }
}

impl eframe::App for OximiteControlApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        apply_theme(ui.ctx());
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(33));

        let snap = self.take_snapshot();
        self.sync_drafts_if_needed(&snap);

        if let Some(ref mut runner) = self.sweep_runner {
            runner.tick(&self.client, &snap.samples);
        }

        self.copy_received_image();
        self.render_top_bar(ui, &snap);
        self.render_status_bar(ui, &snap);

        if matches!(
            self.active_tab,
            AppTab::LiveGraphs | AppTab::Tuning | AppTab::ProfilesSweeps
        ) {
            self.render_sidebar(ui, &snap);
        }

        match self.active_tab {
            AppTab::LiveGraphs => self.render_live_graphs_tab(ui, &snap),
            AppTab::Tuning => self.render_tuning_tab(ui, &snap),
            AppTab::ProfilesSweeps => self.render_profiles_sweeps_tab(ui, &snap),
            AppTab::ReplayCompare => self.render_replay_tab(ui, &snap),
            AppTab::EventLog => self.render_event_log_tab(ui),
        }
    }
}

impl OximiteControlApp {
    fn render_top_bar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::Panel::top("top_nav")
            .frame(
                egui::Frame::new()
                    .fill(Color32::from_rgb(33, 33, 36))
                    .stroke(Stroke::new(1.0, COLOR_BORDER))
                    .inner_margin(egui::Margin::symmetric(12, 6)),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("OXIMITE")
                            .strong()
                            .size(13.5)
                            .color(COLOR_HEALTH),
                    );
                    ui.label(RichText::new("CONTROL").strong().size(13.5));
                    ui.add_space(16.0);

                    for (tab, label) in [
                        (AppTab::LiveGraphs, "Live & Graphs"),
                        (AppTab::Tuning, "PID & Tuning"),
                        (AppTab::ProfilesSweeps, "Profiles & Sweeps"),
                        (AppTab::ReplayCompare, "Replay & Compare"),
                        (AppTab::EventLog, "Event Log"),
                    ] {
                        if ui
                            .selectable_label(
                                self.active_tab == tab,
                                RichText::new(label).strong().size(12.0),
                            )
                            .clicked()
                        {
                            self.active_tab = tab;
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("Clear Buffer").clicked() {
                            if let Ok(mut st) = self.store.lock() {
                                st.clear_samples();
                            }
                            self.viewport.auto_follow = true;
                        }

                        if snap.connected || snap.connecting {
                            if ui
                                .button(RichText::new("Disconnect").color(COLOR_WARN))
                                .clicked()
                            {
                                self.client.disconnect();
                            }
                        } else if ui
                            .button(RichText::new("Connect").strong().color(COLOR_PULSE))
                            .clicked()
                        {
                            self.tuning_initialized = false;
                            self.client.connect(&self.endpoint_input);
                        }

                        ui.add(
                            egui::TextEdit::singleline(&mut self.endpoint_input)
                                .desired_width(150.0)
                                .hint_text("192.168.1.117:8080"),
                        );
                        ui.label(RichText::new("Host:").color(COLOR_DIM_TEXT));
                    });
                });
            });
    }

    fn render_status_bar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::Panel::bottom("status_bar")
            .frame(egui::Frame::new().fill(COLOR_STATUSBAR_BG))
            .show(ui, |ui| {
                ui.add_space(2.0);
                ui.horizontal(|ui| {
                    let s = self.start_time.elapsed().as_secs();
                    let st_txt = |t: String| RichText::new(t).color(Color32::WHITE).size(11.5);
                    ui.label(st_txt(format!(" {:02}:{:02} ", s / 60, s % 60)));
                    ui.separator();
                    ui.label(st_txt(format!("State: {}", snap.state.as_str())));
                    ui.separator();
                    ui.label(st_txt(format!(
                        "Stream: {:.1} Hz | Samples: {} | Dropped: {}",
                        snap.rx_hz,
                        snap.samples.len(),
                        snap.dropped_frames
                    )));
                    if let Some(rtt) = snap.ping_rtt {
                        ui.separator();
                        ui.label(st_txt(format!("Ping: {rtt:.1} ms")));
                    }
                    ui.separator();

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let (status, dot) = if snap.connected {
                            ("CONNECTED", COLOR_GREEN)
                        } else if snap.connecting {
                            ("CONNECTING...", COLOR_CMD)
                        } else {
                            ("OFFLINE", COLOR_WARN)
                        };
                        ui.label(st_txt(format!(" {} ({status}) ", self.endpoint_input)).strong());
                        ui.label(RichText::new("●").color(dot).size(12.0));
                        if let Some(ref err) = snap.last_error {
                            ui.label(
                                RichText::new(err)
                                    .color(Color32::from_rgb(255, 220, 220))
                                    .size(11.0),
                            );
                        }
                    });
                });
                ui.add_space(2.0);
            });
    }

    fn render_sidebar(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::Panel::left("control_sidebar")
            .default_size(275.0)
            .size_range(240.0..=360.0)
            .frame(
                egui::Frame::new()
                    .fill(COLOR_SIDEBAR_BG)
                    .inner_margin(egui::Margin::symmetric(10, 8)),
            )
            .show(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    let f = snap.latest.as_ref().map(|s| &s.frame);
                    let offset = snap
                        .settings
                        .as_ref()
                        .map_or(DEFAULT_SETTINGS.machine.temp_offset, |s| {
                            s.machine.temp_offset
                        });

                    egui::Frame::new()
                        .fill(COLOR_CARD_BG)
                        .stroke(Stroke::new(1.0, COLOR_BORDER))
                        .corner_radius(4.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new("LIVE TELEMETRY")
                                    .strong()
                                    .size(11.5)
                                    .color(COLOR_HEALTH),
                            );
                            ui.add_space(4.0);
                            if let Some(f) = f {
                                let group_t = if snap.state == MachineState::Steaming {
                                    f.t
                                } else {
                                    f.t - offset
                                };
                                egui::Grid::new("telem_grid")
                                    .num_columns(2)
                                    .spacing([12.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("Boiler / Group");
                                        ui.label(
                                            RichText::new(format!("{:.2} / {:.2} C", f.t, group_t))
                                                .strong(),
                                        );
                                        ui.end_row();
                                        ui.label("Target");
                                        ui.label(format!("{:.1} C", f.tt));
                                        ui.end_row();
                                        ui.label("Pressure");
                                        ui.label(
                                            RichText::new(format!("{:.2} / {:.1} bar", f.p, f.tp))
                                                .strong()
                                                .color(COLOR_HEALTH),
                                        );
                                        ui.end_row();
                                        ui.label("Flow / Vol");
                                        ui.label(
                                            RichText::new(format!(
                                                "{:.2} ml/s  ({:.1} ml)",
                                                f.fl, f.vol
                                            ))
                                            .color(COLOR_ACK),
                                        );
                                        ui.end_row();
                                        ui.label("Heater / Pump");
                                        ui.label(format!("{:.0}% / {:.0}%", f.hp, f.pump));
                                        ui.end_row();
                                        ui.label("Constraint");
                                        if f.fc != 0 {
                                            ui.label(
                                                RichText::new("FLOW LIMIT")
                                                    .strong()
                                                    .color(COLOR_ACK),
                                            );
                                        } else {
                                            ui.label(
                                                RichText::new("PRESSURE").color(COLOR_DIM_TEXT),
                                            );
                                        }
                                        ui.end_row();
                                    });
                            } else {
                                ui.label(
                                    RichText::new("No frames yet")
                                        .italics()
                                        .color(COLOR_DIM_TEXT),
                                );
                            }
                        });

                    ui.add_space(8.0);

                    egui::Frame::new()
                        .fill(COLOR_CARD_BG)
                        .stroke(Stroke::new(1.0, COLOR_BORDER))
                        .corner_radius(4.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new("MACHINE CONTROL")
                                    .strong()
                                    .size(11.5)
                                    .color(COLOR_HEALTH),
                            );
                            ui.add_space(4.0);

                            ui.horizontal(|ui| {
                                ui.label("Brew Target:");
                                if ui.button("-0.5").clicked() {
                                    self.client.send_session_temp(snap.sbt - 0.5);
                                }
                                ui.label(RichText::new(format!("{:.1} C", snap.sbt)).strong());
                                if ui.button("+0.5").clicked() {
                                    self.client.send_session_temp(snap.sbt + 0.5);
                                }
                            });

                            ui.add_space(6.0);
                            ui.add_enabled_ui(snap.connected, |ui| {
                                ui.columns(2, |cols| {
                                    if cols[0]
                                        .add_sized(
                                            [cols[0].available_width(), 28.0],
                                            egui::Button::new("BREW"),
                                        )
                                        .clicked()
                                    {
                                        self.client.send_simple("brew");
                                    }
                                    if cols[1]
                                        .add_sized(
                                            [cols[1].available_width(), 28.0],
                                            egui::Button::new("STEAM"),
                                        )
                                        .clicked()
                                    {
                                        self.client.send_simple("steam");
                                    }
                                });
                                ui.add_space(4.0);
                                ui.columns(2, |cols| {
                                    if cols[0]
                                        .add_sized(
                                            [cols[0].available_width(), 28.0],
                                            egui::Button::new("FLUSH"),
                                        )
                                        .clicked()
                                    {
                                        self.client.send_simple("flush");
                                    }
                                    if cols[1]
                                        .add_sized(
                                            [cols[1].available_width(), 28.0],
                                            egui::Button::new("POWER"),
                                        )
                                        .clicked()
                                    {
                                        self.client.send_simple("power");
                                    }
                                });
                                ui.add_space(6.0);
                                let stop_btn = egui::Button::new(
                                    RichText::new("STOP")
                                        .strong()
                                        .color(Color32::WHITE)
                                        .size(13.5),
                                )
                                .fill(Color32::from_rgb(175, 40, 45));
                                if ui
                                    .add_sized([ui.available_width(), 34.0], stop_btn)
                                    .clicked()
                                {
                                    self.client.send_simple("stop");
                                }

                                ui.add_space(6.0);
                                ui.horizontal(|ui| {
                                    ui.add(
                                        egui::Slider::new(&mut self.direct_pump_pct, 0.0..=100.0)
                                            .suffix("%"),
                                    );
                                    if ui.button("Direct").clicked() {
                                        self.client.send_direct_pump(self.direct_pump_pct);
                                    }
                                });

                                ui.add_space(4.0);
                                if ui.button("Ping Device").clicked() {
                                    self.client.send_ping();
                                }
                            });
                        });

                    ui.add_space(8.0);

                    egui::Frame::new()
                        .fill(COLOR_CARD_BG)
                        .stroke(Stroke::new(1.0, COLOR_BORDER))
                        .corner_radius(4.0)
                        .inner_margin(8.0)
                        .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.label(
                                RichText::new("GRAPH WINDOW & EXPORT")
                                    .strong()
                                    .size(11.5)
                                    .color(COLOR_HEALTH),
                            );
                            ui.add_space(4.0);
                            if self.active_tab == AppTab::Tuning {
                                ui.horizontal(|ui| {
                                    for (lbl, secs) in
                                        [("15s", 15.0), ("60s", 60.0), ("5m", 300.0), ("All", 0.0)]
                                    {
                                        let active = (self.viewport.window_secs - secs).abs() < 0.1
                                            && self.viewport.auto_follow;
                                        if ui.selectable_label(active, lbl).clicked() {
                                            self.viewport.window_secs = secs;
                                            self.viewport.auto_follow = true;
                                        }
                                    }
                                });
                                ui.checkbox(&mut self.viewport.auto_follow, "Auto-follow latest");
                            }
                            ui.checkbox(&mut self.show_stats, "Show Temp Stats overlay");
                            ui.horizontal(|ui| {
                                if self.active_tab == AppTab::LiveGraphs
                                    && ui.button("Copy session image").clicked()
                                {
                                    self.live_ctx_ppp = ui.ctx().pixels_per_point();
                                    self.request_session_image(ui.ctx());
                                }
                            });
                        });
                });
            });
    }

    fn render_live_graphs_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(COLOR_MAIN_BG)
                    .inner_margin(MAIN_PANEL_MARGIN),
            )
            .show(ui, |ui| {
                self.live_rect = ui.max_rect();
                render_live_3panel(
                    ui,
                    &snap.brew_samples,
                    &mut self.brew_viewport,
                    self.show_stats,
                );
            });
    }

    fn request_session_image(&self, ctx: &egui::Context) {
        let tx = self.image_tx.clone();
        ctx.request_screenshot(move |img| {
            let _ = tx.send(img);
        });
    }

    fn copy_received_image(&mut self) {
        let Ok(full) = self.image_rx.try_recv() else {
            return;
        };
        let img = full.region(&self.live_rect, Some(self.live_ctx_ppp));
        let data = arboard::ImageData {
            width: img.width(),
            height: img.height(),
            bytes: std::borrow::Cow::Borrowed(img.as_raw()),
        };
        let result = arboard::Clipboard::new().and_then(|mut c| c.set_image(data));
        if let Ok(mut st) = self.store.lock() {
            let msg = match result {
                Ok(()) => "Session image copied to clipboard".to_string(),
                Err(e) => format!("Clipboard copy failed: {e}"),
            };
            st.add_log(LogTag::Settings, msg, String::new());
        }
    }

    fn render_tuning_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(COLOR_MAIN_BG)
                    .inner_margin(MAIN_PANEL_MARGIN),
            )
            .show(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("PID & Machine Tuning").strong().size(15.0));
                        ui.add_space(12.0);
                        if ui.button("Reload from Device").clicked() {
                            self.tuning_initialized = false;
                            self.client.request_settings();
                        }
                        if let Some(ref msg) = self.tuning_status {
                            ui.label(RichText::new(msg).color(COLOR_PULSE));
                        }
                    });
                    if snap.state.is_busy() {
                        ui.label(
                            RichText::new(
                                "Machine is busy: settings saves wait until Idle before applying.",
                            )
                            .color(COLOR_CMD),
                        );
                    }
                    ui.add_space(6.0);

                    ui.columns(2, |cols| {
                        egui::Frame::new()
                            .fill(COLOR_CARD_BG)
                            .stroke(Stroke::new(1.0, COLOR_BORDER))
                            .corner_radius(4.0)
                            .inner_margin(10.0)
                            .show(&mut cols[0], |ui| {
                                ui.label(
                                    RichText::new("Machine Settings")
                                        .strong()
                                        .color(COLOR_HEALTH),
                                );
                                egui::Grid::new("machine_grid")
                                    .num_columns(2)
                                    .spacing([12.0, 6.0])
                                    .show(ui, |ui| {
                                        let m = &mut self.draft_machine;
                                        ui.label("Brew Temp (C)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.brew_temp)
                                                .speed(0.1)
                                                .range(0.0..=130.0),
                                        );
                                        ui.end_row();
                                        ui.label("Steam Temp (C)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.steam_temp)
                                                .speed(0.5)
                                                .range(0.0..=165.0),
                                        );
                                        ui.end_row();
                                        ui.label("Temp Offset (C)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.temp_offset)
                                                .speed(0.1)
                                                .range(-20.0..=60.0),
                                        );
                                        ui.end_row();
                                        ui.label("Steam Limit (s)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.steam_time_limit_s)
                                                .speed(1.0)
                                                .range(0.0..=600.0),
                                        );
                                        ui.end_row();
                                        ui.label("Feed-Forward (%)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.feed_forward_percents)
                                                .speed(1.0)
                                                .range(0.0..=200.0),
                                        );
                                        ui.end_row();
                                        ui.label("Flow (edges/L)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.flow_pulses_per_liter)
                                                .speed(100.0)
                                                .range(1.0..=1_000_000.0),
                                        );
                                        ui.end_row();
                                        ui.label("Sleep Timeout (min)");
                                        ui.add(
                                            egui::DragValue::new(&mut m.sleep_timeout_min)
                                                .speed(1.0)
                                                .range(0.0..=1440.0),
                                        );
                                        ui.end_row();
                                    });
                                ui.add_space(6.0);
                                if ui.button("Apply & Save Machine Settings").clicked() {
                                    self.client.send_save_machine(self.draft_machine);
                                    self.tuning_status = Some("Sent save_machine".into());
                                }
                            });

                        egui::Frame::new()
                            .fill(COLOR_CARD_BG)
                            .stroke(Stroke::new(1.0, COLOR_BORDER))
                            .corner_radius(4.0)
                            .inner_margin(10.0)
                            .show(&mut cols[1], |ui| {
                                ui.label(RichText::new("PID Gains").strong().color(COLOR_HEALTH));
                                egui::Grid::new("pid_grid")
                                    .num_columns(4)
                                    .spacing([10.0, 6.0])
                                    .show(ui, |ui| {
                                        ui.label("Temp PID");
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_temp_pid.kp)
                                                .prefix("Kp ")
                                                .speed(0.1)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_temp_pid.ki)
                                                .prefix("Ki ")
                                                .speed(0.01)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_temp_pid.kd)
                                                .prefix("Kd ")
                                                .speed(0.1)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.end_row();

                                        ui.label("Pump PID");
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_pump_pid.kp)
                                                .prefix("Kp ")
                                                .speed(0.1)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_pump_pid.ki)
                                                .prefix("Ki ")
                                                .speed(0.1)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.add(
                                            egui::DragValue::new(&mut self.draft_pump_pid.kd)
                                                .prefix("Kd ")
                                                .speed(0.1)
                                                .range(0.0..=1000.0),
                                        );
                                        ui.end_row();
                                    });
                                ui.add_space(6.0);
                                if ui.button("Apply & Save PIDs").clicked() {
                                    self.client
                                        .send_save_pids(self.draft_temp_pid, self.draft_pump_pid);
                                    self.tuning_status = Some("Sent save_pids".into());
                                }

                                ui.add_space(10.0);
                                ui.separator();
                                ui.label(
                                    RichText::new("Wi-Fi Credentials")
                                        .strong()
                                        .color(COLOR_HEALTH),
                                );
                                ui.horizontal(|ui| {
                                    ui.label("SSID:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.draft_wifi_ssid)
                                            .desired_width(140.0),
                                    );
                                    ui.label("Pass:");
                                    ui.add(
                                        egui::TextEdit::singleline(&mut self.draft_wifi_pass)
                                            .password(true)
                                            .desired_width(140.0),
                                    );
                                });
                                if ui.button("Save Wi-Fi").clicked() {
                                    match self.client.send_save_wifi(
                                        &self.draft_wifi_ssid,
                                        &self.draft_wifi_pass,
                                    ) {
                                        Ok(_) => {
                                            self.draft_wifi_pass.clear();
                                            self.tuning_status = Some("Sent save_wifi".into());
                                        }
                                        Err(e) => self.tuning_status = Some(e),
                                    }
                                }
                            });
                    });

                    ui.add_space(8.0);
                    ui.set_min_height(340.0);
                    render_live_3panel(ui, &snap.samples, &mut self.viewport, self.show_stats);
                });
            });
    }

    fn render_profiles_sweeps_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(COLOR_MAIN_BG)
                    .inner_margin(MAIN_PANEL_MARGIN),
            )
            .show(ui, |ui| {
                ScrollArea::vertical().show(ui, |ui| {
                    ui.columns(2, |cols| {
                        // Left: Saved Profiles & Editor
                        egui::Frame::new()
                            .fill(COLOR_CARD_BG)
                            .stroke(Stroke::new(1.0, COLOR_BORDER))
                            .corner_radius(4.0)
                            .inner_margin(10.0)
                            .show(&mut cols[0], |ui| {
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new("Saved Profiles").strong().color(COLOR_HEALTH));
                                    if ui.button("Refresh").clicked() {
                                        self.client.request_profiles();
                                    }
                                });
                                ui.add_space(4.0);
                                if snap.profiles.is_empty() {
                                    ui.label(RichText::new("No saved profiles reported.").color(COLOR_DIM_TEXT));
                                } else {
                                    for hdr in &snap.profiles {
                                        ui.horizontal(|ui| {
                                            ui.label(format!("[{}]", hdr.slot));
                                            ui.label(RichText::new(hdr.name.as_str()).strong());
                                            if ui.button("Run").clicked() {
                                                self.client.send_run_slot(hdr.slot);
                                            }
                                            if ui.button("Edit").clicked() {
                                                self.last_loaded_profile_name = None;
                                                self.client.request_profile(hdr.slot);
                                            }
                                            if ui
                                                .button(RichText::new("Del").color(COLOR_WARN))
                                                .clicked()
                                            {
                                                self.client.send_delete_profile(hdr.slot);
                                            }
                                        });
                                    }
                                }

                                ui.separator();
                                ui.label(RichText::new("Profile Editor").strong().color(COLOR_HEALTH));
                                ui.horizontal(|ui| {
                                    ui.label("Slot:");
                                    egui::ComboBox::from_id_salt("prof_slot_cb")
                                        .selected_text(format!("Slot {}", self.prof_slot))
                                        .show_ui(ui, |ui| {
                                            for s in 0..MAX_PROFILES {
                                                ui.selectable_value(
                                                    &mut self.prof_slot,
                                                    s,
                                                    format!("Slot {s}"),
                                                );
                                            }
                                        });
                                    ui.label("Name:");
                                    ui.add(egui::TextEdit::singleline(&mut self.prof_name).desired_width(160.0));
                                });

                                ui.add_space(4.0);
                                let mut remove_idx = None;
                                egui::Grid::new("step_editor")
                                    .num_columns(5)
                                    .spacing([6.0, 4.0])
                                    .show(ui, |ui| {
                                        ui.label("Time (s)");
                                        ui.label("Vol (ml)");
                                        ui.label("Press (bar)");
                                        ui.label("Flow (ml/s)");
                                        ui.label("");
                                        ui.end_row();
                                        for (i, st) in self.prof_steps.iter_mut().enumerate() {
                                            let mut t = st.time_s.unwrap_or(0.0);
                                            let mut v = st.volume.unwrap_or(0.0);
                                            let mut p = st.pressure.unwrap_or(0.0);
                                            let mut fl = st.flow.unwrap_or(0.0);
                                            ui.add(egui::DragValue::new(&mut t).speed(0.5).range(0.0..=600.0));
                                            ui.add(egui::DragValue::new(&mut v).speed(1.0).range(0.0..=2000.0));
                                            ui.add(egui::DragValue::new(&mut p).speed(0.5).range(0.0..=100.0));
                                            ui.add(egui::DragValue::new(&mut fl).speed(0.2).range(0.0..=20.0));
                                            st.time_s = (t > 0.0).then_some(t);
                                            st.volume = (v > 0.0).then_some(v);
                                            st.pressure = (p > 0.0).then_some(p);
                                            st.flow = (fl > 0.0).then_some(fl);
                                            if ui.button("X").clicked() {
                                                remove_idx = Some(i);
                                            }
                                            ui.end_row();
                                        }
                                    });
                                if let Some(idx) = remove_idx {
                                    self.prof_steps.remove(idx);
                                }

                                ui.horizontal(|ui| {
                                    if self.prof_steps.len() < MAX_STEPS && ui.button("+ Add Step").clicked() {
                                        self.prof_steps.push(BrewProfileStep {
                                            time_s: Some(10.0),
                                            volume: None,
                                            pressure: Some(9.0),
                                            flow: None,
                                        });
                                    }
                                    if let Ok(name) = oximite_proto::heapless::String::try_from(self.prof_name.trim()) {
                                        let mut steps = oximite_proto::heapless::Vec::new();
                                        for s in self.prof_steps.iter().take(MAX_STEPS) {
                                            let _ = steps.push(s.clone());
                                        }
                                        let prof = BrewProfile { name, steps };
                                        if ui.button("Run (No Save)").clicked() {
                                            self.client.send_run_profile(&prof);
                                        }
                                        if ui.button("Save to Slot").clicked() {
                                            self.client.send_save_profile(self.prof_slot, prof);
                                        }
                                    } else {
                                        ui.label(RichText::new("Name > 32 bytes").color(COLOR_WARN));
                                    }
                                });
                            });

                        // Right: Automated Bench Sweeps
                        egui::Frame::new()
                            .fill(COLOR_CARD_BG)
                            .stroke(Stroke::new(1.0, COLOR_BORDER))
                            .corner_radius(4.0)
                            .inner_margin(10.0)
                            .show(&mut cols[1], |ui| {
                                ui.label(RichText::new("Automated Bench Sweeps").strong().color(COLOR_HEALTH));
                                let running = self.sweep_runner.as_ref().is_some_and(|r| !r.finished);

                                ui.add_enabled_ui(!running && snap.connected, |ui| {
                                    ui.horizontal(|ui| {
                                        ui.label("Temp Step: base");
                                        ui.add(egui::DragValue::new(&mut self.sweep_temp_base).range(40.0..=110.0));
                                        ui.label("-> target");
                                        ui.add(egui::DragValue::new(&mut self.sweep_temp_target).range(60.0..=130.0));
                                        ui.label("cool(s)");
                                        ui.add(egui::DragValue::new(&mut self.sweep_temp_cool_s).range(5..=120));
                                        ui.label("rec(s)");
                                        ui.add(egui::DragValue::new(&mut self.sweep_temp_rec_s).range(10..=300));
                                        if ui.button("Start").clicked() {
                                            self.sweep_runner = Some(SweepRunner::start(
                                                SweepConfig::TempStep {
                                                    base_temp: self.sweep_temp_base,
                                                    target_temp: self.sweep_temp_target,
                                                    cooldown_s: self.sweep_temp_cool_s,
                                                    record_s: self.sweep_temp_rec_s,
                                                    pid: self.draft_temp_pid,
                                                },
                                                &self.client,
                                                self.draft_machine,
                                                snap.samples.len(),
                                            ));
                                        }
                                    });

                                    ui.horizontal(|ui| {
                                        ui.label("Pressure Step:");
                                        ui.add(egui::DragValue::new(&mut self.sweep_press_low).range(0.5..=12.0));
                                        ui.label("->");
                                        ui.add(egui::DragValue::new(&mut self.sweep_press_high).range(1.0..=12.0));
                                        if ui.button("Start").clicked() {
                                            self.sweep_runner = Some(SweepRunner::start(
                                                SweepConfig::PressureStep {
                                                    low_bar: self.sweep_press_low,
                                                    high_bar: self.sweep_press_high,
                                                    step1_s: 3.0,
                                                    step2_s: 8.0,
                                                    pid: self.draft_pump_pid,
                                                },
                                                &self.client,
                                                self.draft_machine,
                                                snap.samples.len(),
                                            ));
                                        }
                                    });

                                    ui.horizontal(|ui| {
                                        ui.label("Pump Curve: step%");
                                        ui.add(egui::DragValue::new(&mut self.sweep_pump_step_pct).range(5..=25));
                                        ui.label("hold(s)");
                                        ui.add(egui::DragValue::new(&mut self.sweep_pump_hold_s).range(1.0..=10.0));
                                        if ui.button("Start").clicked() {
                                            self.sweep_runner = Some(SweepRunner::start(
                                                SweepConfig::PumpPowerCurve {
                                                    step_pct: self.sweep_pump_step_pct,
                                                    hold_s: self.sweep_pump_hold_s,
                                                },
                                                &self.client,
                                                self.draft_machine,
                                                snap.samples.len(),
                                            ));
                                        }
                                    });

                                    ui.horizontal(|ui| {
                                        ui.label("Boiler Stability (s):");
                                        ui.add(egui::DragValue::new(&mut self.sweep_stability_s).range(10..=1800));
                                        if ui.button("Start").clicked() {
                                            self.sweep_runner = Some(SweepRunner::start(
                                                SweepConfig::BoilerStability {
                                                    duration_s: self.sweep_stability_s,
                                                },
                                                &self.client,
                                                self.draft_machine,
                                                snap.samples.len(),
                                            ));
                                        }
                                    });
                                });

                                if let Some(ref mut runner) = self.sweep_runner {
                                    ui.add_space(6.0);
                                    ui.add(egui::ProgressBar::new(runner.progress).text(&runner.status));
                                    if !runner.finished && ui.button("Abort Sweep").clicked() {
                                        runner.abort(&self.client);
                                    }
                                    if let Some(ref res) = runner.result {
                                        ui.add_space(4.0);
                                        match res {
                                            SweepResult::TempStep(st) | SweepResult::Stability(st) => {
                                                ui.label(format!(
                                                    "N={} | Mean={:.2} C | Min={:.2} | Max={:.2} | SD={:.3} C",
                                                    st.count, st.mean, st.min, st.max, st.std_dev
                                                ));
                                            }
                                            SweepResult::PressureStep { samples, max_bar } => {
                                                ui.label(format!("Samples={samples} | Peak={max_bar:.2} bar"));
                                            }
                                            SweepResult::PumpCurve(pts) => {
                                                for pt in pts {
                                                    ui.label(format!(
                                                        "{:>3}% -> {:.2} ml/s, {:.2} bar",
                                                        pt.power_pct, pt.mean_flow_ml_s, pt.mean_pressure_bar
                                                    ));
                                                }
                                            }
                                        }
                                    }
                                }
                            });
                    });
                });
            });
    }

    fn render_replay_tab(&mut self, ui: &mut egui::Ui, snap: &Snapshot) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(COLOR_MAIN_BG)
                    .inner_margin(MAIN_PANEL_MARGIN),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Load Trace A (.jsonl)").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("JSONL", &["jsonl"])
                            .pick_file()
                        {
                            match import_samples_jsonl(&p) {
                                Ok(t) => {
                                    self.cmp_viewport.x_min = 0.0;
                                    self.cmp_viewport.x_max =
                                        t.samples.last().map_or(30.0, |s| s.t_s.max(5.0));
                                    self.trace_a = Some(t);
                                    self.replay_status = None;
                                }
                                Err(e) => self.replay_status = Some(e),
                            }
                        }
                    }
                    if ui.button("Load Trace B (.jsonl)").clicked() {
                        if let Some(p) = rfd::FileDialog::new()
                            .add_filter("JSONL", &["jsonl"])
                            .pick_file()
                        {
                            match import_samples_jsonl(&p) {
                                Ok(t) => {
                                    self.trace_b = Some(t);
                                    self.replay_status = None;
                                }
                                Err(e) => self.replay_status = Some(e),
                            }
                        }
                    }
                    if let Some(ref mut b) = self.trace_b {
                        ui.label("B Offset (s):");
                        ui.add(
                            egui::DragValue::new(&mut b.offset_s)
                                .speed(0.1)
                                .range(-60.0..=60.0),
                        );
                    }
                    if let Some(ref err) = self.replay_status {
                        ui.label(RichText::new(err).color(COLOR_WARN));
                    }
                });

                if !snap.shots.is_empty() {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("Captured Shots:");
                        for shot in &snap.shots {
                            if ui.button(format!("A: {}", shot.label)).clicked() {
                                let t0 = shot.samples.first().map_or(0.0, |s| s.t_s);
                                let norm: Vec<TelemetrySample> = shot
                                    .samples
                                    .iter()
                                    .map(|s| TelemetrySample {
                                        t_s: s.t_s - t0,
                                        frame: s.frame,
                                    })
                                    .collect();
                                self.cmp_viewport.x_min = 0.0;
                                self.cmp_viewport.x_max =
                                    norm.last().map_or(30.0, |s| s.t_s.max(5.0));
                                self.trace_a = Some(LoadedTrace {
                                    name: shot.label.clone(),
                                    samples: norm,
                                    offset_s: 0.0,
                                });
                            }
                        }
                    });
                }

                ui.add_space(6.0);
                render_comparison_panels(
                    ui,
                    self.trace_a.as_ref(),
                    self.trace_b.as_ref(),
                    &mut self.cmp_viewport,
                );
            });
    }

    fn render_event_log_tab(&mut self, ui: &mut egui::Ui) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::new()
                    .fill(COLOR_MAIN_BG)
                    .inner_margin(MAIN_PANEL_MARGIN),
            )
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (tag, lbl) in [
                        (None, "ALL"),
                        (Some(LogTag::Cmd), "CMD"),
                        (Some(LogTag::State), "STATE"),
                        (Some(LogTag::Step), "STEP"),
                        (Some(LogTag::Settings), "CFG"),
                        (Some(LogTag::Net), "NET"),
                    ] {
                        if ui.selectable_label(self.log_filter == tag, lbl).clicked() {
                            self.log_filter = tag;
                        }
                    }
                    ui.separator();
                    ui.label("Search:");
                    ui.add(egui::TextEdit::singleline(&mut self.log_search).desired_width(150.0));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(RichText::new("Clear Log").color(COLOR_WARN))
                            .clicked()
                        {
                            if let Ok(mut st) = self.store.lock() {
                                st.clear_logs();
                            }
                            self.log_view = LogView::default();
                        }
                        ui.checkbox(&mut self.log_show_raw, "Show Raw");
                        ui.checkbox(&mut self.log_newest_first, "Newest First");
                    });
                });
                ui.add_space(6.0);
                egui::Frame::new()
                    .fill(Color32::from_rgb(24, 24, 24))
                    .stroke(Stroke::new(1.0, COLOR_BORDER))
                    .corner_radius(4.0)
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        let font = FontId::monospace(LOG_FONT_SIZE);
                        let row_h = ui.fonts_mut(|f| f.row_height(&font));
                        let rows = &self.log_view.rows;
                        ScrollArea::both().auto_shrink([false; 2]).show_rows(
                            ui,
                            row_h,
                            rows.len(),
                            |ui, range| {
                                ui.style_mut().wrap_mode = Some(TextWrapMode::Extend);
                                for e in &rows[range] {
                                    ui.horizontal(|ui| {
                                        let ts = e.timestamp.format("%H:%M:%S%.3f").to_string();
                                        ui.label(
                                            RichText::new(ts)
                                                .font(font.clone())
                                                .color(COLOR_DIM_TEXT),
                                        );
                                        let col = match e.tag {
                                            LogTag::Cmd => COLOR_CMD,
                                            LogTag::State => COLOR_PULSE,
                                            LogTag::Step => COLOR_ACK,
                                            LogTag::Settings => COLOR_HEALTH,
                                            LogTag::Net => COLOR_TEXT,
                                        };
                                        ui.label(
                                            RichText::new(format!("[{:<5}]", e.tag.as_str()))
                                                .font(font.clone())
                                                .color(col)
                                                .strong(),
                                        );
                                        ui.label(RichText::new(&e.summary).font(font.clone()));
                                        if self.log_show_raw && !e.raw.is_empty() {
                                            ui.label(
                                                RichText::new(format!("| {}", e.raw))
                                                    .font(font.clone())
                                                    .color(COLOR_DIM_TEXT),
                                            );
                                        }
                                    });
                                }
                            },
                        );
                    });
            });
    }
}
