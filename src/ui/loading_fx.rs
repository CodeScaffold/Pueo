//! Animated background behind the sidebar header while the connection is working: a faint
//! grid of memory cells that a soft scan wave lights up as it passes, with the odd bit
//! flipping on and off. One colour, fades in and out smoothly.

use std::cell::Cell;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{cairo, glib};

use crate::db::log::QueryLog;

/// How often the connection is checked for activity while nothing is animating.
const POLL: Duration = Duration::from_millis(40);
/// Even an instant query plays for at least this long, so it doesn't just flicker.
const MIN_SHOW: f64 = 0.9;
/// Fade speeds (per second).
const FADE_IN: f64 = 7.0;
const FADE_OUT: f64 = 2.2;
/// Distance between cells, and a cell's size.
const PITCH: f64 = 7.0;
const CELL: f64 = 1.6;
/// Seconds for the scan wave to cross the header.
const SCAN: f64 = 2.2;

struct State {
    log: Arc<QueryLog>,
    /// Connection colour; the system accent colour when it has none.
    color: Option<(f64, f64, f64)>,
    /// 0 (hidden) to 1 (fully shown).
    intensity: Cell<f64>,
    /// Animation clock in seconds, and the frame time it was last advanced at.
    time: Cell<f64>,
    last_frame: Cell<Option<i64>>,
    last_started: Cell<u64>,
    /// Keep showing until this animation time, for queries that finish instantly.
    hold_until: Cell<f64>,
    ticking: Cell<bool>,
}

pub fn new(log: Arc<QueryLog>, color: Option<(f64, f64, f64)>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::builder().hexpand(true).vexpand(true).can_target(false).build();
    let state = Rc::new(State {
        last_started: Cell::new(log.activity().1),
        log,
        color,
        intensity: Cell::new(0.0),
        time: Cell::new(0.0),
        last_frame: Cell::new(None),
        hold_until: Cell::new(0.0),
        ticking: Cell::new(false),
    });

    let s = state.clone();
    area.set_draw_func(move |_, cr, w, h| draw(&s, cr, w as f64, h as f64));

    // Wake up on activity; the frame clock takes over while animating.
    let weak = area.downgrade();
    glib::timeout_add_local(POLL, move || {
        let Some(area) = weak.upgrade() else { return glib::ControlFlow::Break };
        if !state.ticking.get() && state.poll() {
            start(&area, state.clone());
        }
        glib::ControlFlow::Continue
    });
    area
}

impl State {
    /// Note new activity; true when there is something to show.
    fn poll(&self) -> bool {
        let (running, started) = self.log.activity();
        if started != self.last_started.replace(started) {
            self.hold_until.set(self.time.get() + MIN_SHOW);
        }
        running > 0 || self.time.get() < self.hold_until.get()
    }
}

fn start(area: &gtk::DrawingArea, state: Rc<State>) {
    state.ticking.set(true);
    state.last_frame.set(None);
    area.add_tick_callback(move |area, clock| {
        let now = clock.frame_time();
        let dt = state.last_frame.replace(Some(now)).map_or(0.0, |last| (now - last) as f64 / 1e6).min(0.1);
        state.time.set(state.time.get() + dt);
        let target = if state.poll() { 1.0 } else { 0.0 };
        let current = state.intensity.get();
        let rate = if target > current { FADE_IN } else { FADE_OUT };
        let next = current + (target - current) * (dt * rate).min(1.0);
        area.queue_draw();
        if target == 0.0 && next < 0.005 {
            state.intensity.set(0.0);
            state.ticking.set(false);
            return glib::ControlFlow::Break;
        }
        state.intensity.set(next);
        glib::ControlFlow::Continue
    });
}

/// Stable pseudo-random value in 0..1.
fn hash(n: f64) -> f64 {
    ((n * 12.9898).sin() * 43_758.545_3).rem_euclid(1.0)
}

fn smoothstep(x: f64) -> f64 {
    let x = x.clamp(0.0, 1.0);
    x * x * (3.0 - 2.0 * x)
}

fn draw(state: &State, cr: &cairo::Context, w: f64, h: f64) {
    let i = smoothstep(state.intensity.get());
    if i <= 0.001 || w < 1.0 || h < 1.0 {
        return;
    }
    let style = adw::StyleManager::default();
    let dark = style.is_dark();
    let (r, g, b) = state.color.unwrap_or_else(|| {
        let a = style.accent_color_rgba();
        (a.red() as f64, a.green() as f64, a.blue() as f64)
    });
    // Without animations (accessibility setting) the grid shows, but holds still.
    let animate = gtk::Settings::default().is_none_or(|s| s.is_gtk_enable_animations());
    let t = if animate { state.time.get() } else { 0.0 };

    // The scan wave: a soft band travelling left to right, with a faint wash of colour.
    let band = (w * 0.3).max(40.0);
    let wave = if animate { ((t % SCAN) / SCAN) * (w + 2.0 * band) - band } else { w * 0.5 };
    let wash = cairo::LinearGradient::new(wave - band, 0.0, wave + band, 0.0);
    wash.add_color_stop_rgba(0.0, r, g, b, 0.0);
    wash.add_color_stop_rgba(0.5, r, g, b, if dark { 0.10 } else { 0.08 } * i);
    wash.add_color_stop_rgba(1.0, r, g, b, 0.0);
    let _ = cr.set_source(&wash);
    cr.rectangle(0.0, 0.0, w, h);
    let _ = cr.fill();

    // The cells, centred in the header. Bits flip a few times a second.
    let cols = (w / PITCH).floor() as i64;
    let rows = (h / PITCH).floor() as i64;
    let (x0, y0) = ((w - (cols - 1) as f64 * PITCH) / 2.0, (h - (rows - 1) as f64 * PITCH) / 2.0);
    let tick = (t * 6.0).floor();
    let rest = if dark { 0.10 } else { 0.12 };
    for row in 0..rows {
        for col in 0..cols {
            let (x, y) = (x0 + col as f64 * PITCH, y0 + row as f64 * PITCH);
            let seed = (col * 131 + row * 71) as f64;
            let d = (x - wave) / band;
            let lit = (-d * d * 4.0).exp() * (0.45 + 0.55 * hash(seed));
            let bit = if hash(seed + tick * 0.37) > 0.965 { 0.5 } else { 0.0 };
            let alpha = (rest + 0.55 * lit + bit).min(0.7) * i;
            cr.set_source_rgba(r, g, b, alpha);
            cr.rectangle(x - CELL / 2.0, y - CELL / 2.0, CELL, CELL);
            let _ = cr.fill();
        }
    }
}
