//! Custom synchronized `egui::Painter` multi-panel graphing widget.
//!
//! Supports linked X-axes across stacked panels, dual Y-axes per panel,
//! wheel zoom, drag panning, hover crosshairs, shaded `fc==1` constraint bands,
//! bucket decimation for large 50 Hz captures, and shot comparison overlays.

use egui::{Align2, Color32, FontId, Pos2, Rect, Sense, Stroke, Ui};

use crate::store::{compute_temp_stats, LoadedTrace, TelemetrySample};

#[derive(Clone, Debug)]
pub struct PlotViewport {
    pub x_min: f64,
    pub x_max: f64,
    pub auto_follow: bool,
    pub window_secs: f64,
}

impl Default for PlotViewport {
    fn default() -> Self {
        Self {
            x_min: 0.0,
            x_max: 60.0,
            auto_follow: true,
            window_secs: 60.0,
        }
    }
}

impl PlotViewport {
    pub fn sync_to_samples(&mut self, samples: &[TelemetrySample]) {
        let Some(last) = samples.last() else {
            self.x_min = 0.0;
            self.x_max = self.window_secs;
            return;
        };
        if self.auto_follow {
            let first_t = samples.first().map_or(0.0, |s| s.t_s);
            let span = self.window_secs.max(1.0);
            if self.window_secs <= 0.0 {
                self.x_min = first_t;
                self.x_max = (last.t_s + 0.5).max(first_t + 5.0);
            } else {
                self.x_max = (last.t_s + 0.5).max(first_t + span);
                self.x_min = (self.x_max - span).max(first_t);
            }
        }
    }
}

#[derive(Clone, Copy)]
pub struct TraceLine<'a> {
    pub label: &'a str,
    pub color: Color32,
    pub width: f32,
    pub right_axis: bool,
    pub extract: fn(&TelemetrySample) -> f32,
}

pub struct PanelBounds {
    pub left_min: f32,
    pub left_max: f32,
    pub left_label: &'static str,
    pub right_min: f32,
    pub right_max: f32,
    pub right_label: &'static str,
    pub title: &'static str,
}

fn handle_pan_zoom(
    ui: &Ui,
    plot_rect: Rect,
    response: &egui::Response,
    vp: &mut PlotViewport,
    hover_x: &mut Option<f64>,
) {
    if response.double_clicked() {
        vp.auto_follow = true;
    }

    if response.dragged() {
        let dx = response.drag_delta().x as f64;
        if dx.abs() > 0.0 && plot_rect.width() > 1.0 {
            let span = (vp.x_max - vp.x_min).max(0.1);
            let shift = -(dx / f64::from(plot_rect.width())) * span;
            vp.x_min += shift;
            vp.x_max += shift;
            vp.auto_follow = false;
        }
    }

    if response.hovered() {
        let scroll_y = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll_y.abs() > 0.1 {
            let factor = (1.0 - f64::from(scroll_y) * 0.002).clamp(0.5, 1.5);
            let span = (vp.x_max - vp.x_min).max(0.5);
            let new_span = (span * factor).clamp(1.0, 7200.0);
            let anchor_frac = response
                .hover_pos()
                .map(|p| f64::from((p.x - plot_rect.left()) / plot_rect.width()).clamp(0.0, 1.0))
                .unwrap_or(0.5);
            let anchor_x = vp.x_min + span * anchor_frac;
            vp.x_min = anchor_x - new_span * anchor_frac;
            vp.x_max = vp.x_min + new_span;
            vp.auto_follow = false;
        }
        if let Some(pos) = response.hover_pos() {
            if plot_rect.contains(pos) {
                let frac = f64::from((pos.x - plot_rect.left()) / plot_rect.width());
                *hover_x = Some(vp.x_min + frac * (vp.x_max - vp.x_min));
            }
        }
    }
}

fn map_x(t: f64, rect: Rect, vp: &PlotViewport) -> f32 {
    let span = (vp.x_max - vp.x_min).max(1e-6);
    rect.left() + ((t - vp.x_min) / span) as f32 * rect.width()
}

fn map_y(v: f32, min: f32, max: f32, rect: Rect) -> f32 {
    let span = (max - min).max(1e-4);
    let norm = ((v - min) / span).clamp(0.0, 1.0);
    rect.bottom() - norm * rect.height()
}

fn visible_slice<'a>(samples: &'a [TelemetrySample], vp: &PlotViewport) -> &'a [TelemetrySample] {
    let start = samples
        .partition_point(|s| s.t_s < vp.x_min)
        .saturating_sub(1);
    let end = (samples.partition_point(|s| s.t_s <= vp.x_max) + 1).min(samples.len());
    if start >= end {
        &[]
    } else {
        &samples[start..end]
    }
}

fn decimated_points<F>(
    slice: &[TelemetrySample],
    plot_rect: Rect,
    vp: &PlotViewport,
    y_min: f32,
    y_max: f32,
    extract: &F,
) -> Vec<Pos2>
where
    F: Fn(&TelemetrySample) -> f32,
{
    let max_pts = (plot_rect.width() as usize * 2).max(600);
    let mut pts = Vec::with_capacity(slice.len().min(max_pts));
    if slice.len() <= max_pts {
        for s in slice {
            pts.push(Pos2::new(
                map_x(s.t_s, plot_rect, vp),
                map_y(extract(s), y_min, y_max, plot_rect),
            ));
        }
        return pts;
    }

    let step = (slice.len() / max_pts).max(1);
    for chunk in slice.chunks(step) {
        let mut min_s = &chunk[0];
        let mut max_s = &chunk[0];
        let mut min_v = extract(min_s);
        let mut max_v = min_v;
        for s in chunk.iter().skip(1) {
            let v = extract(s);
            if v < min_v {
                min_v = v;
                min_s = s;
            }
            if v > max_v {
                max_v = v;
                max_s = s;
            }
        }
        if min_s.t_s <= max_s.t_s {
            pts.push(Pos2::new(
                map_x(min_s.t_s, plot_rect, vp),
                map_y(min_v, y_min, y_max, plot_rect),
            ));
            pts.push(Pos2::new(
                map_x(max_s.t_s, plot_rect, vp),
                map_y(max_v, y_min, y_max, plot_rect),
            ));
        } else {
            pts.push(Pos2::new(
                map_x(max_s.t_s, plot_rect, vp),
                map_y(max_v, y_min, y_max, plot_rect),
            ));
            pts.push(Pos2::new(
                map_x(min_s.t_s, plot_rect, vp),
                map_y(min_v, y_min, y_max, plot_rect),
            ));
        }
    }
    pts
}

#[allow(clippy::too_many_arguments)]
fn draw_panel(
    ui: &mut Ui,
    height: f32,
    samples: &[TelemetrySample],
    overlay: Option<(&[TelemetrySample], f64)>,
    vp: &mut PlotViewport,
    bounds: &PanelBounds,
    lines: &[TraceLine<'_>],
    shade_flow_limit: bool,
    hover_x: &mut Option<f64>,
    extra_badge: Option<&str>,
) {
    let width = ui.available_width().max(240.0);
    let (outer, response) =
        ui.allocate_exact_size(egui::vec2(width, height), Sense::click_and_drag());
    let plot_rect = Rect::from_min_max(
        Pos2::new(outer.left() + 52.0, outer.top() + 20.0),
        Pos2::new(outer.right() - 48.0, outer.bottom() - 18.0),
    );

    handle_pan_zoom(ui, plot_rect, &response, vp, hover_x);

    let painter = ui.painter_at(outer);
    painter.rect_filled(outer, 4.0, Color32::from_rgb(24, 24, 26));
    painter.rect_stroke(
        outer,
        4.0,
        Stroke::new(1.0, Color32::from_rgb(55, 55, 60)),
        egui::StrokeKind::Inside,
    );

    // Title + Axis labels
    painter.text(
        Pos2::new(plot_rect.left(), outer.top() + 4.0),
        Align2::LEFT_TOP,
        bounds.title,
        FontId::proportional(11.5),
        Color32::from_rgb(210, 210, 210),
    );
    painter.text(
        Pos2::new(outer.left() + 4.0, outer.top() + 4.0),
        Align2::LEFT_TOP,
        bounds.left_label,
        FontId::proportional(10.0),
        Color32::from_rgb(150, 150, 160),
    );
    painter.text(
        Pos2::new(outer.right() - 4.0, outer.top() + 4.0),
        Align2::RIGHT_TOP,
        bounds.right_label,
        FontId::proportional(10.0),
        Color32::from_rgb(150, 150, 160),
    );

    // Horizontal & vertical grid
    for i in 0..=4 {
        let frac = i as f32 / 4.0;
        let y = plot_rect.bottom() - frac * plot_rect.height();
        painter.line_segment(
            [
                Pos2::new(plot_rect.left(), y),
                Pos2::new(plot_rect.right(), y),
            ],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 120, 130, 35)),
        );
        let lv = bounds.left_min + frac * (bounds.left_max - bounds.left_min);
        painter.text(
            Pos2::new(plot_rect.left() - 4.0, y),
            Align2::RIGHT_CENTER,
            format!("{lv:.0}"),
            FontId::monospace(9.5),
            Color32::from_rgb(155, 155, 165),
        );
        let rv = bounds.right_min + frac * (bounds.right_max - bounds.right_min);
        painter.text(
            Pos2::new(plot_rect.right() + 4.0, y),
            Align2::LEFT_CENTER,
            format!("{rv:.0}"),
            FontId::monospace(9.5),
            Color32::from_rgb(155, 155, 165),
        );
    }

    for i in 0..=6 {
        let frac = i as f64 / 6.0;
        let x = plot_rect.left() + frac as f32 * plot_rect.width();
        painter.line_segment(
            [
                Pos2::new(x, plot_rect.top()),
                Pos2::new(x, plot_rect.bottom()),
            ],
            Stroke::new(1.0, Color32::from_rgba_unmultiplied(120, 120, 130, 30)),
        );
        let t_val = vp.x_min + frac * (vp.x_max - vp.x_min);
        painter.text(
            Pos2::new(x, plot_rect.bottom() + 2.0),
            Align2::CENTER_TOP,
            format!("{t_val:.1}s"),
            FontId::monospace(9.5),
            Color32::from_rgb(145, 145, 155),
        );
    }

    let plot_painter = ui.painter_at(plot_rect);
    let slice = visible_slice(samples, vp);

    if shade_flow_limit && slice.len() > 1 {
        let mut band_start: Option<f32> = None;
        for s in slice {
            let x = map_x(s.t_s, plot_rect, vp);
            if s.frame.fc != 0 {
                if band_start.is_none() {
                    band_start = Some(x);
                }
            } else if let Some(x0) = band_start.take() {
                plot_painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(x0, plot_rect.top()),
                        Pos2::new(x.max(x0 + 1.5), plot_rect.bottom()),
                    ),
                    0.0,
                    Color32::from_rgba_unmultiplied(187, 134, 252, 28),
                );
            }
        }
        if let Some(x0) = band_start {
            if let Some(last) = slice.last() {
                let x = map_x(last.t_s, plot_rect, vp);
                plot_painter.rect_filled(
                    Rect::from_min_max(
                        Pos2::new(x0, plot_rect.top()),
                        Pos2::new(x.max(x0 + 1.5), plot_rect.bottom()),
                    ),
                    0.0,
                    Color32::from_rgba_unmultiplied(187, 134, 252, 28),
                );
            }
        }
    }

    for line in lines {
        let (ymin, ymax) = if line.right_axis {
            (bounds.right_min, bounds.right_max)
        } else {
            (bounds.left_min, bounds.left_max)
        };
        let pts = decimated_points(slice, plot_rect, vp, ymin, ymax, &line.extract);
        if pts.len() >= 2 {
            plot_painter.add(egui::Shape::line(pts, Stroke::new(line.width, line.color)));
        }
    }

    if let Some((ov_samples, offset_s)) = overlay {
        let shifted_vp = PlotViewport {
            x_min: vp.x_min - offset_s,
            x_max: vp.x_max - offset_s,
            auto_follow: false,
            window_secs: vp.window_secs,
        };
        let ov_slice = visible_slice(ov_samples, &shifted_vp);
        for line in lines {
            let (ymin, ymax) = if line.right_axis {
                (bounds.right_min, bounds.right_max)
            } else {
                (bounds.left_min, bounds.left_max)
            };
            let pts: Vec<Pos2> =
                decimated_points(ov_slice, plot_rect, &shifted_vp, ymin, ymax, &line.extract);
            if pts.len() >= 2 {
                let dim = Color32::from_rgba_unmultiplied(
                    line.color.r(),
                    line.color.g(),
                    line.color.b(),
                    115,
                );
                plot_painter.add(egui::Shape::line(pts, Stroke::new(1.3, dim)));
            }
        }
    }

    // Legend top-right inside plot
    let mut lx = plot_rect.right() - 6.0;
    for line in lines.iter().rev() {
        let text = line.label;
        let galley =
            painter.layout_no_wrap(text.to_string(), FontId::proportional(10.0), line.color);
        let w = galley.size().x;
        painter.galley(Pos2::new(lx - w, outer.top() + 3.0), galley, line.color);
        lx -= w + 12.0;
    }

    if let Some(badge) = extra_badge {
        painter.text(
            Pos2::new(plot_rect.left() + 8.0, plot_rect.top() + 6.0),
            Align2::LEFT_TOP,
            badge,
            FontId::monospace(10.5),
            Color32::from_rgb(235, 220, 150),
        );
    }

    // Hover crosshair & value readout
    if let Some(hx) = *hover_x {
        if hx >= vp.x_min && hx <= vp.x_max {
            let px = map_x(hx, plot_rect, vp);
            plot_painter.line_segment(
                [
                    Pos2::new(px, plot_rect.top()),
                    Pos2::new(px, plot_rect.bottom()),
                ],
                Stroke::new(1.0, Color32::from_rgba_unmultiplied(255, 255, 255, 110)),
            );
            let idx = slice.partition_point(|s| s.t_s < hx);
            if let Some(s) = slice.get(idx.min(slice.len().saturating_sub(1))) {
                let summary: Vec<String> = lines
                    .iter()
                    .map(|l| format!("{}={:.2}", l.label, (l.extract)(s)))
                    .collect();
                painter.text(
                    Pos2::new(plot_rect.left() + 8.0, plot_rect.bottom() - 4.0),
                    Align2::LEFT_BOTTOM,
                    format!("@{:.2}s  {}", s.t_s, summary.join("  ")),
                    FontId::monospace(10.0),
                    Color32::from_rgb(230, 230, 230),
                );
            }
        }
    }
}

pub fn render_live_3panel(
    ui: &mut Ui,
    samples: &[TelemetrySample],
    vp: &mut PlotViewport,
    show_stats: bool,
) {
    vp.sync_to_samples(samples);
    let mut hover_x = None;
    let total_h = ui.available_height().max(360.0);
    let each_h = ((total_h - 12.0) / 3.0).max(115.0);

    let slice = visible_slice(samples, vp);
    let temp_badge = if show_stats {
        let refs: Vec<&TelemetrySample> = slice.iter().collect();
        compute_temp_stats(&refs).map(|st| {
            format!(
                "Mean {:.2}C | Min {:.2} | Max {:.2} | SD {:.3}",
                st.mean, st.min, st.max, st.std_dev
            )
        })
    } else {
        None
    };

    let mut t_min = 15.0f32;
    let mut t_max = 115.0f32;
    if !slice.is_empty() {
        let lo = slice
            .iter()
            .map(|s| s.frame.t.min(s.frame.tt))
            .fold(f32::INFINITY, f32::min);
        let hi = slice
            .iter()
            .map(|s| s.frame.t.max(s.frame.tt))
            .fold(f32::NEG_INFINITY, f32::max);
        t_min = (lo - 5.0).floor().max(0.0);
        t_max = (hi + 5.0).ceil().max(t_min + 10.0);
    }

    let temp_lines = [
        TraceLine {
            label: "Target",
            color: Color32::from_rgb(130, 130, 140),
            width: 1.2,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.tt,
        },
        TraceLine {
            label: "Heater%",
            color: Color32::from_rgb(230, 150, 70),
            width: 1.2,
            right_axis: true,
            extract: |s: &TelemetrySample| s.frame.hp,
        },
        TraceLine {
            label: "Temp",
            color: Color32::from_rgb(244, 71, 71),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.t,
        },
    ];
    draw_panel(
        ui,
        each_h,
        samples,
        None,
        vp,
        &PanelBounds {
            left_min: t_min,
            left_max: t_max,
            left_label: "C",
            right_min: 0.0,
            right_max: 100.0,
            right_label: "SSR %",
            title: "Temperature & Heater Duty",
        },
        &temp_lines,
        false,
        &mut hover_x,
        temp_badge.as_deref(),
    );

    ui.add_space(4.0);

    let press_lines = [
        TraceLine {
            label: "TargetBar",
            color: Color32::from_rgb(130, 130, 140),
            width: 1.2,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.tp,
        },
        TraceLine {
            label: "Pump%",
            color: Color32::from_rgb(180, 130, 90),
            width: 1.2,
            right_axis: true,
            extract: |s: &TelemetrySample| s.frame.pump,
        },
        TraceLine {
            label: "Pressure",
            color: Color32::from_rgb(79, 193, 255),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.p,
        },
    ];
    draw_panel(
        ui,
        each_h,
        samples,
        None,
        vp,
        &PanelBounds {
            left_min: 0.0,
            left_max: 12.0,
            left_label: "Bar",
            right_min: 0.0,
            right_max: 100.0,
            right_label: "Pump %",
            title: "Pressure & Pump Duty (shaded = flow-limited)",
        },
        &press_lines,
        true,
        &mut hover_x,
        None,
    );

    ui.add_space(4.0);

    let max_vol = slice
        .iter()
        .map(|s| s.frame.vol)
        .fold(40.0f32, f32::max)
        .ceil();
    let flow_lines = [
        TraceLine {
            label: "TargetFlow",
            color: Color32::from_rgb(130, 130, 140),
            width: 1.2,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.tfl,
        },
        TraceLine {
            label: "Volume",
            color: Color32::from_rgb(78, 201, 176),
            width: 1.5,
            right_axis: true,
            extract: |s: &TelemetrySample| s.frame.vol,
        },
        TraceLine {
            label: "Flow",
            color: Color32::from_rgb(187, 134, 252),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.fl,
        },
    ];
    draw_panel(
        ui,
        each_h,
        samples,
        None,
        vp,
        &PanelBounds {
            left_min: 0.0,
            left_max: 8.0,
            left_label: "ml/s",
            right_min: 0.0,
            right_max: max_vol,
            right_label: "ml",
            title: "Flow Rate & Cumulative Volume",
        },
        &flow_lines,
        false,
        &mut hover_x,
        None,
    );
}

pub fn render_comparison_panels(
    ui: &mut Ui,
    trace_a: Option<&LoadedTrace>,
    trace_b: Option<&LoadedTrace>,
    vp: &mut PlotViewport,
) {
    let base: &[TelemetrySample] = trace_a.map_or(&[], |t| &t.samples);
    let ov = trace_b.map(|t| (t.samples.as_slice(), t.offset_s));
    let mut hover_x = None;
    let total_h = ui.available_height().max(320.0);
    let each_h = ((total_h - 12.0) / 3.0).max(105.0);

    let temp_lines = [
        TraceLine {
            label: "Temp",
            color: Color32::from_rgb(244, 71, 71),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.t,
        },
        TraceLine {
            label: "Target",
            color: Color32::from_rgb(130, 130, 140),
            width: 1.2,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.tt,
        },
    ];
    draw_panel(
        ui,
        each_h,
        base,
        ov,
        vp,
        &PanelBounds {
            left_min: 20.0,
            left_max: 140.0,
            left_label: "C",
            right_min: 0.0,
            right_max: 100.0,
            right_label: "%",
            title: "Comparison: Temperature (solid = A, faded = B)",
        },
        &temp_lines,
        false,
        &mut hover_x,
        None,
    );
    ui.add_space(4.0);

    let press_lines = [
        TraceLine {
            label: "Pressure",
            color: Color32::from_rgb(79, 193, 255),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.p,
        },
        TraceLine {
            label: "Pump%",
            color: Color32::from_rgb(180, 130, 90),
            width: 1.2,
            right_axis: true,
            extract: |s: &TelemetrySample| s.frame.pump,
        },
    ];
    draw_panel(
        ui,
        each_h,
        base,
        ov,
        vp,
        &PanelBounds {
            left_min: 0.0,
            left_max: 12.0,
            left_label: "Bar",
            right_min: 0.0,
            right_max: 100.0,
            right_label: "Pump %",
            title: "Comparison: Pressure & Pump",
        },
        &press_lines,
        false,
        &mut hover_x,
        None,
    );
    ui.add_space(4.0);

    let flow_lines = [
        TraceLine {
            label: "Flow",
            color: Color32::from_rgb(187, 134, 252),
            width: 2.0,
            right_axis: false,
            extract: |s: &TelemetrySample| s.frame.fl,
        },
        TraceLine {
            label: "Volume",
            color: Color32::from_rgb(78, 201, 176),
            width: 1.5,
            right_axis: true,
            extract: |s: &TelemetrySample| s.frame.vol,
        },
    ];
    draw_panel(
        ui,
        each_h,
        base,
        ov,
        vp,
        &PanelBounds {
            left_min: 0.0,
            left_max: 8.0,
            left_label: "ml/s",
            right_min: 0.0,
            right_max: 100.0,
            right_label: "ml",
            title: "Comparison: Flow & Volume",
        },
        &flow_lines,
        false,
        &mut hover_x,
        None,
    );
}
