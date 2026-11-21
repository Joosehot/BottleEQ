//! The window (steps 3 to 5): a small one that stays on top, with ONE graph from 20 Hz to 20 kHz:
//! the live spectrum of what the target is given (green, its level on the right-hand scale), and over
//! it the equalizer's curve (amber, its gain on the left-hand scale) with its handles to drag.
//!
//! - **The small squares** are the ten bands: drag one up or down for that band's gain. A mode sets them.
//! - **The round points** are free, as in FL Studio's EQ: drag one sideways for its frequency, up and
//!   down for its gain. The lowest is a shelf (it raises or lowers that end), the highest too, the ones
//!   between are peaks.
//! - **Wheel over a point**: its width (a narrow peak is a spike).
//! - **Right button on a point**: its kind (a slope at the start or the end, a shelf, a peak, a spike),
//!   or take it away. **Right button or double click on the empty graph**: a new point there.
//! - **Flat** puts the bands and the points back to nothing: the sound passes as it came.
//! - **Measure** plays a sweep out of the speakers and records it with the microphone chosen
//!   (`measure.rs`); what reached the microphone is drawn in blue on the curve's scale, dotted where it
//!   is not clearly above the noise. It can be measured again at any time; the line is the last one.
//!
//! The sound changes while a handle is dragged: the curve goes to the audio thread through `Settings`
//! (atomics) at every move, and the audio thread slides to it. The audio thread never waits for the
//! window; its sound comes back for the spectrum through a `Ring`. Drawn with egui (eframe).

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use eframe::egui::{self, pos2, Color32, FontId, Pos2, Rect, Sense, Stroke};
use crate::eq::{self, Curve, Eq, Kind, Mode, Point, Settings, BANDS, CENTRES, CUT_Q, MOST, POINTS, POINT_Q};
use crate::ring::Ring;
use crate::spectrum::{Spectrum, SIZE};
use crate::audio::{self, run, Input, Output, Running};
use crate::measure::{self, Measured};

/// the graph's edges: frequencies, the curve's dB each way, the spectrum's lowest level
const LOW: f32 = 20.0;
const HIGH: f32 = 20000.0;
const SPAN: f32 = 18.0;
const DEEP: f32 = -72.0;
/// how near a point the pointer takes hold of it, in screen points
const REACH: f32 = 14.0;
/// a spike's width
const SPIKE_Q: f32 = 6.0;

const AMBER: Color32 = Color32::from_rgb(255, 190, 80);
const GREEN: Color32 = Color32::from_rgb(70, 190, 130);
const BLUE: Color32 = Color32::from_rgb(110, 175, 255);
/// how loud the sweep is sent, of the most there is
const SWEEP: f32 = 0.5;

/// How the window is opened.
pub struct Start {
    pub from: Option<String>,
    pub to: Option<String>,
    /// Some: begin at once in that mode (it was asked for on the command line); None: wait for Start
    pub exclusive: Option<bool>,
    pub cushion_ms: f32,
    pub settings: Arc<Settings>,
    pub mode: Option<String>,
    /// for trying it out: a WAV played to the source at a level, a point dragged by itself to a
    /// frequency and gain, and a picture of the window kept after some seconds, after which it closes
    pub play: Option<(String, f32)>,
    pub drag: Option<(usize, f32, f32)>,
    pub shot: Option<(String, f32)>,
    /// the microphone, and whether to measure by itself a second after opening
    pub mic: Option<String>,
    pub measure: bool,
}

/// The points a curve starts from: one at each end and one between (each between two bands), all at 0 dB.
pub fn first_points() -> [Point; 3] {
    [Point::new(90.0, 0.0), Point::new(1400.0, 0.0), Point::new(11000.0, 0.0)]
}

/// What the pointer has hold of: one of the ten bands or one of the points.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Hold {
    Band(usize),
    Point(usize),
}

fn x_of(graph: Rect, hz: f32) -> f32 {
    graph.left() + graph.width() * (hz / LOW).log2() / (HIGH / LOW).log2()
}

fn hz_at(graph: Rect, x: f32) -> f32 {
    (LOW * (HIGH / LOW).powf((x - graph.left()) / graph.width())).clamp(LOW, HIGH)
}

fn y_of(graph: Rect, db: f32) -> f32 {
    graph.center().y - db.clamp(-SPAN, SPAN) / SPAN * graph.height() / 2.0
}

fn db_at(graph: Rect, y: f32) -> f32 {
    ((graph.center().y - y) / (graph.height() / 2.0) * SPAN).clamp(-MOST, MOST)
}

/// Where point `i` is held: at its frequency and gain (a cut has no gain: on the 0 dB line).
fn handle(graph: Rect, curve: &Curve, i: usize) -> Pos2 {
    let p = curve.points()[i];
    pos2(x_of(graph, p.hz), y_of(graph, if matches!(curve.kind(i), Kind::LowCut | Kind::HighCut) { 0.0 } else { p.db }))
}

/// Where band `b` is held: at its centre and gain.
fn band_handle(graph: Rect, curve: &Curve, b: usize) -> Pos2 {
    pos2(x_of(graph, CENTRES[b]), y_of(graph, curve.gains[b]))
}

/// What the pointer would take hold of: the nearest handle within reach. A point lying on a band is
/// taken first (the band is reached by moving the point off it).
fn nearest(graph: Rect, curve: &Curve, at: Pos2) -> Option<Hold> {
    let points = (0..curve.points().len()).map(|i| (Hold::Point(i), handle(graph, curve, i).distance(at)));
    let bands = (0..BANDS).map(|b| (Hold::Band(b), band_handle(graph, curve, b).distance(at) + 3.0));
    points.chain(bands).filter(|h| h.1 <= REACH).min_by(|a, b| a.1.total_cmp(&b.1)).map(|h| h.0)
}

/// A gain as the pointer sets it: in tenths of a dB, and near the 0 dB line exactly on it (nothing is
/// the one gain worth hitting exactly).
fn snapped(db: f32) -> f32 {
    if db.abs() < 0.3 { 0.0 } else { (db * 10.0).round() / 10.0 }
}

/// The points with point `i` dragged to a place in the graph.
fn dragged(graph: Rect, curve: &Curve, i: usize, to: Pos2) -> Vec<Point> {
    let mut points = curve.points().to_vec();
    points[i].hz = hz_at(graph, to.x);
    if !matches!(curve.kind(i), Kind::LowCut | Kind::HighCut) {
        points[i].db = snapped(db_at(graph, to.y));
    }
    points
}

fn hz_text(hz: f32) -> String {
    if hz >= 1000.0 { format!("{:.1} kHz", hz / 1000.0) } else { format!("{hz:.0} Hz") }
}

fn point_text(curve: &Curve, i: usize) -> String {
    let p = curve.points()[i];
    match curve.kind(i) {
        Kind::LowCut => format!("{}  slope at the start (width {:.1})", hz_text(p.hz), p.q),
        Kind::HighCut => format!("{}  slope at the end (width {:.1})", hz_text(p.hz), p.q),
        Kind::LowShelf | Kind::HighShelf => format!("{}  {:+.1} dB  shelf", hz_text(p.hz), p.db),
        Kind::Peak => format!("{}  {:+.1} dB  {} (width {:.1})", hz_text(p.hz), p.db, if p.q >= 3.0 { "spike" } else { "peak" }, p.q),
    }
}

struct Window {
    outputs: Vec<Output>,
    source: Option<usize>,
    target: Option<usize>,
    exclusive: bool,
    cushion_ms: f32,
    settings: Arc<Settings>,
    running: Option<Running>,
    /// the sound the target is given (one channel), from the audio thread
    ring: Arc<Ring>,
    spectrum: Spectrum,
    modes: Vec<Mode>,
    /// the mode the bands were set from; empty when they were moved by hand
    mode: String,
    on_top: bool,
    /// the handle being dragged, and the point (and place) the menu was opened on
    held: Option<Hold>,
    menu: (Option<usize>, Pos2),
    graph: Rect,
    /// what the path says of itself, looked at twice a second
    looked: Instant,
    looks: u32,
    delay: (f32, f32),
    said: String,
    // trying it out
    opened: Instant,
    play: Option<(String, f32)>,
    drag: Option<(usize, f32, f32)>,
    drag_step: usize,
    shot: Option<(String, f32)>,
    shot_asked: bool,
    // measuring
    inputs: Vec<Input>,
    mic: Option<usize>,
    /// a measurement under way: where its result comes, and when it began
    measuring: Option<(std::sync::mpsc::Receiver<Result<Measured, String>>, Instant)>,
    measured: Option<Measured>,
    measure_said: String,
    measure_soon: bool,
}

impl Window {
    /// Stop the path if it runs, and start it between the chosen devices.
    fn start(&mut self) {
        self.running = None;
        let (Some(s), Some(t)) = (self.source, self.target) else {
            self.said = "choose where the sound is captured from and where it is played".to_string();
            return;
        };
        let (source, target) = (self.outputs[s].clone(), self.outputs[t].clone());
        let (mut equalizer, g, ring) = (Eq::new(target.rate), self.settings.clone(), self.ring.clone());
        let process = move |sound: &mut [f32], channels: usize| {
            equalizer.process(sound, channels, &g);
            // one channel of it for the spectrum; if the window is not looking, it is dropped
            let mut one = [0f32; 256];
            for frames in sound.chunks(one.len() * channels) {
                let n = frames.len() / channels;
                one.iter_mut().zip(frames.chunks(channels)).for_each(|(o, f)| *o = f.iter().sum::<f32>() / channels as f32);
                ring.push(&one[..n]);
            }
        };
        match run(&source, &target, self.cushion_ms, self.exclusive, |_, _| {}, process) {
            Ok(running) => {
                self.said = if self.exclusive && !running.opened.exclusive { format!("exclusive mode was not to be had ({}): shared, with a long delay", running.opened.note) } else { String::new() };
                if let Some((file, level)) = self.play.take() {
                    match crate::read_wav(&file) {
                        Ok((rate, mut music)) if rate == source.rate => {
                            music.iter_mut().for_each(|s| *s *= level);
                            audio::play(&source, music);
                        }
                        Ok((rate, _)) => self.said = format!("{file} is at {rate} Hz and the source at {} Hz", source.rate),
                        Err(e) => self.said = format!("{file}: {e}"),
                    }
                }
                self.running = Some(running);
            }
            Err(e) => self.said = e,
        }
    }

    /// What stands in the way of hearing anything, said in the graph where it is seen.
    fn notice(&self) -> Option<String> {
        if !self.said.is_empty() {
            return Some(self.said.clone());
        }
        let (Some(s), Some(_)) = (self.source, self.target) else {
            return Some("Choose where the sound is captured from (From) and the speakers it is played to (to).".to_string());
        };
        // Linux: Bottle EQ's own device is there only while it runs, so first Start, then choose it
        if cfg!(target_os = "linux") && self.running.is_none() {
            return Some("Press Start.".to_string());
        }
        if !self.outputs[s].default {
            let plays = self.outputs.iter().find(|o| o.default).map_or("another device".to_string(), |o| format!("\"{}\"", o.name));
            let (name, system) = (if cfg!(windows) { self.outputs[s].name.as_str() } else { "Bottle EQ" }, audio::SYSTEM);
            let where_ = if cfg!(windows) { "the speaker icon on the taskbar" } else { "the sound settings" };
            return Some(format!("{system} plays to {plays}, not to \"{name}\".\nOnly what plays to \"{name}\" passes through Bottle EQ:\nchoose it as the output device ({where_})."));
        }
        self.running.is_none().then(|| "Press Start.".to_string())
    }

    fn rate(&self) -> f64 {
        self.target.map_or(48000.0, |t| self.outputs[t].rate as f64)
    }

    fn top_rows(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            match self.running.is_some() {
                true => {
                    if ui.button("Stop").clicked() {
                        self.running = None;
                    }
                }
                false => {
                    if ui.button("Start").clicked() {
                        self.start();
                    }
                }
            }
            let name = |outputs: &[Output], i: Option<usize>| i.map_or("choose".to_string(), |i| outputs[i].name.clone());
            let was = (self.source, self.target, self.exclusive);
            ui.add_enabled_ui(self.running.is_none(), |ui| {
                ui.label("From");
                egui::ComboBox::from_id_salt("from").width(170.0).selected_text(name(&self.outputs, self.source)).show_ui(ui, |ui| {
                    // Linux: the sound comes from Bottle EQ's own device, the first; no other can be captured
                    for (i, o) in self.outputs.iter().enumerate().filter(|(i, _)| cfg!(windows) || *i == 0) {
                        ui.selectable_value(&mut self.source, Some(i), format!("{}{}", o.name, if o.default { format!("  ({} plays here)", audio::SYSTEM) } else { String::new() }));
                    }
                });
                ui.label("to");
                egui::ComboBox::from_id_salt("to").width(170.0).selected_text(name(&self.outputs, self.target)).show_ui(ui, |ui| {
                    // every device but the source: its own sound cannot be played back to it
                    for (i, o) in self.outputs.iter().enumerate().filter(|(i, _)| Some(*i) != self.source) {
                        ui.selectable_value(&mut self.target, Some(i), o.name.clone());
                    }
                });
                if self.target == self.source {
                    self.target = None;
                }
                // Linux has no such mode: the graph is shared by all and the delay is short as it is
                if cfg!(windows) {
                    ui.checkbox(&mut self.exclusive, "exclusive").on_hover_text("Take the target for Bottle EQ alone: the delay is then about 6 ms instead of 44, and nothing else can play to that device meanwhile.");
                }
            });
            if was != (self.source, self.target, self.exclusive) {
                self.said.clear();
            }
        });
        ui.horizontal(|ui| {
            ui.label("Mode");
            let shown = if self.mode.is_empty() { "by hand".to_string() } else { self.mode.clone() };
            egui::ComboBox::from_id_salt("mode").width(130.0).height(420.0).selected_text(shown).show_ui(ui, |ui| {
                for mode in &self.modes {
                    if ui.selectable_label(self.mode == mode.name, &mode.name).clicked() {
                        self.settings.set_all(&mode.gains);
                        self.mode = mode.name.clone();
                    }
                }
            });
            if ui.button("Flat").on_hover_text("Every band at 0 dB and the points back where they began: the sound passes as it came.").clicked() {
                self.settings.set_all(&[0.0; BANDS]);
                self.settings.set_points(&first_points());
                self.mode = "Flat".to_string();
            }
            let mut on = self.settings.on.load(Ordering::Relaxed);
            if ui.checkbox(&mut on, "on").on_hover_text("Off: the sound passes untouched whatever is set, to hear the difference.").changed() {
                self.settings.on.store(on, Ordering::Relaxed);
            }
            if ui.checkbox(&mut self.on_top, "stay on top").changed() {
                let level = if self.on_top { egui::WindowLevel::AlwaysOnTop } else { egui::WindowLevel::Normal };
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::WindowLevel(level));
            }
        });
        ui.horizontal(|ui| {
            ui.label("Microphone");
            let name = self.mic.map_or("none".to_string(), |i| self.inputs[i].name.clone());
            ui.add_enabled_ui(self.measuring.is_none(), |ui| {
                egui::ComboBox::from_id_salt("mic").width(190.0).selected_text(name).show_ui(ui, |ui| {
                    for (i, input) in self.inputs.iter().enumerate() {
                        ui.selectable_value(&mut self.mic, Some(i), input.name.clone());
                    }
                });
                let button = ui.button(if self.measured.is_some() { "Measure again" } else { "Measure" }).on_hover_text(
                    "Plays a sweep from 20 Hz to 20 kHz out of the speakers (about 15 seconds, loud) and records it with the microphone: put the microphone where you listen, and pause the music first.\nThe blue line is what reached the microphone: speakers, room and microphone together.",
                );
                if button.clicked() {
                    self.measure();
                }
            });
            let text = match &self.measuring {
                Some((_, began)) => format!("measuring: keep quiet ... {:.0} s", (measure::BEFORE + measure::SECONDS + measure::AFTER + 1.0 - began.elapsed().as_secs_f64()).max(0.0)),
                None => self.measure_said.clone(),
            };
            ui.label(egui::RichText::new(text).small());
        });
    }

    /// Begin a measurement: the sweep goes where music goes. While the path runs that is its source,
    /// so the sweep passes the equalizer as it is set; else it is played straight to the speakers.
    fn measure(&mut self) {
        let (Some(mic), Some(target)) = (self.mic, self.target) else {
            self.measure_said = "choose a microphone, and the speakers to play to (to)".to_string();
            return;
        };
        let play_to = match (&self.running, self.source) {
            (Some(_), Some(source)) => self.outputs[source].clone(),
            _ => self.outputs[target].clone(),
        };
        let (said, heard) = std::sync::mpsc::channel();
        let input = self.inputs[mic].clone();
        std::thread::spawn(move || {
            let _ = said.send(measure::measure(&input, &play_to, SWEEP));
        });
        self.measuring = Some((heard, Instant::now()));
    }

    /// Take a measurement's result when it has come.
    fn measured(&mut self) {
        let Some((heard, _)) = &self.measuring else { return };
        match heard.try_recv() {
            Ok(Ok(m)) => {
                let clipped = if m.peak >= 0.99 { "; THE MICROPHONE CLIPPED: turn it down and measure again" } else { "" };
                self.measure_said = format!("measured: {:.0}% clearly above the noise{clipped}", 100.0 * m.share());
                (self.measured, self.measuring) = (Some(m), None);
            }
            Ok(Err(why)) => (self.measure_said, self.measuring) = (why, None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => (self.measure_said, self.measuring) = ("the measurement broke off".to_string(), None),
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
    }

    fn bottom_rows(&mut self, ui: &mut egui::Ui) {
        // what the path says of itself
        let text = match (&self.running, self.said.is_empty()) {
            (_, false) => self.said.clone(),
            (Some(r), _) => {
                let m = &r.meters;
                format!(
                    "delay {:.1} ms (longest {:.1})   turned down {:.1} dB for headroom   gaps {}",
                    self.delay.0,
                    self.delay.1,
                    if self.settings.on.load(Ordering::Relaxed) { -self.settings.get().headroom(self.rate()) } else { 0.0 },
                    m.dry.load(Ordering::Relaxed) + m.dropped.load(Ordering::Relaxed)
                )
            }
            (None, _) => "stopped".to_string(),
        };
        ui.label(egui::RichText::new(text).small());
    }

    fn graph(&mut self, ui: &mut egui::Ui) {
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        // room around the graph for the scales: gain on the left, level on the right, frequency below
        let graph = Rect::from_min_max(response.rect.min + egui::vec2(46.0, 20.0), response.rect.max - egui::vec2(40.0, 34.0));
        self.graph = graph;
        painter.rect_filled(graph, 2.0, Color32::from_gray(18));
        let (faint, small, grey) = (Stroke::new(1.0, Color32::from_gray(44)), FontId::proportional(11.0), Color32::from_gray(150));
        for (hz, name) in [(20.0, "20"), (50.0, "50"), (100.0, "100"), (200.0, "200"), (500.0, "500"), (1000.0, "1k"), (2000.0, "2k"), (5000.0, "5k"), (10000.0, "10k"), (20000.0, "20k")] {
            let x = x_of(graph, hz);
            painter.line_segment([pos2(x, graph.top()), pos2(x, graph.bottom())], faint);
            painter.text(pos2(x, graph.bottom() + 4.0), egui::Align2::CENTER_TOP, name, small.clone(), grey);
        }
        painter.text(pos2(graph.center().x, graph.bottom() + 18.0), egui::Align2::CENTER_TOP, "frequency (Hz)", small.clone(), grey);
        // left: the curve's gain, in the curve's colour
        for db in [-18.0, -12.0, -6.0, 0.0, 6.0, 12.0, 18.0f32] {
            let y = y_of(graph, db);
            if db.abs() < SPAN {
                painter.line_segment([pos2(graph.left(), y), pos2(graph.right(), y)], if db == 0.0 { Stroke::new(1.0, Color32::from_gray(84)) } else { faint });
            }
            painter.text(pos2(graph.left() - 6.0, y), egui::Align2::RIGHT_CENTER, if db == 0.0 { "0".to_string() } else { format!("{db:+.0}") }, small.clone(), AMBER);
        }
        painter.text(pos2(response.rect.left() + 2.0, graph.top() - 18.0), egui::Align2::LEFT_TOP, "EQ (dB): above 0 louder, below quieter", small.clone(), AMBER);
        // right: the spectrum's level, in the spectrum's colour
        for level in [0.0, -24.0, -48.0, -72.0f32] {
            let y = graph.bottom() - (level - DEEP) / -DEEP * graph.height();
            painter.text(pos2(graph.right() + 6.0, y), egui::Align2::LEFT_CENTER, format!("{level:.0}"), small.clone(), GREEN);
        }
        painter.text(pos2(response.rect.right() - 2.0, graph.top() - 18.0), egui::Align2::RIGHT_TOP, "the sound now (dB)", small.clone(), GREEN);
        let painter = painter.with_clip_rect(graph);
        // what the frequencies are, in words
        for (from, to, name) in [(20.0, 60.0, "sub bass"), (60.0, 250.0, "bass"), (250.0, 2000.0, "mids"), (2000.0, 6000.0, "upper mids"), (6000.0, 20000.0, "treble")] {
            let (left, right) = (x_of(graph, from), x_of(graph, to));
            if to < HIGH {
                painter.line_segment([pos2(right, graph.top()), pos2(right, graph.top() + 16.0)], Stroke::new(1.0, Color32::from_gray(110)));
            }
            painter.text(pos2((left + right) / 2.0, graph.top() + 3.0), egui::Align2::CENTER_TOP, name, small.clone(), Color32::from_gray(170));
        }

        // the sound now: at each two points across, the level of the sixth of an octave around that
        // frequency, so it is one smooth line (level for sound with as much power in every octave)
        let rate = self.rate() as f32;
        let sixth = 2f32.powf(1.0 / 12.0);
        let steps = (graph.width() / 2.0) as usize;
        let mut line = Vec::with_capacity(steps + 1);
        for step in 0..=steps {
            let x = graph.left() + step as f32 * 2.0;
            let hz = hz_at(graph, x);
            let level = self.spectrum.band(hz / sixth, hz * sixth, rate);
            let y = graph.bottom() - ((level - DEEP) / -DEEP).clamp(0.0, 1.0) * graph.height();
            painter.line_segment([pos2(x, graph.bottom()), pos2(x, y)], Stroke::new(2.0, GREEN.gamma_multiply(0.28)));
            line.push(pos2(x, y));
        }
        painter.line(line, Stroke::new(2.0, GREEN));

        // what was measured at the microphone, on the curve's scale: whole where it is clearly above
        // the noise, dotted where it is not
        if let Some(m) = &self.measured {
            // not held at the graph's edge as the curve is: what is weaker than the graph shows leaves it
            let at = |i: usize| pos2(x_of(graph, m.hz[i]), graph.center().y - m.db[i] / SPAN * graph.height() / 2.0);
            for i in 1..m.hz.len() {
                match m.certain[i - 1] && m.certain[i] {
                    true => drop(painter.line_segment([at(i - 1), at(i)], Stroke::new(2.0, BLUE))),
                    false if i % 3 == 0 => drop(painter.line_segment([at(i - 1), at(i)], Stroke::new(1.5, BLUE.gamma_multiply(0.6)))),
                    false => {}
                }
            }
            painter.text(pos2(graph.left() + 6.0, graph.bottom() - 4.0), egui::Align2::LEFT_BOTTOM, "blue: measured at the microphone (dotted: not clearly above the noise)", small.clone(), BLUE);
        }

        // the pointer and the handles
        let curve = self.settings.get();
        let pointer = response.hover_pos().or(response.interact_pointer_pos());
        let under = pointer.and_then(|at| nearest(graph, &curve, at));
        if response.drag_started() {
            self.held = response.interact_pointer_pos().and_then(|at| nearest(graph, &curve, at));
        }
        match (self.held, response.interact_pointer_pos(), response.dragged()) {
            (Some(Hold::Point(i)), Some(at), true) => self.settings.set_points(&dragged(graph, &curve, i, at)),
            (Some(Hold::Band(b)), Some(at), true) => {
                self.settings.set(b, snapped(db_at(graph, at.y)));
                self.mode.clear();
            }
            _ => {}
        }
        if response.drag_stopped() {
            self.held = None;
        }
        let wheel = ui.input(|i| i.smooth_scroll_delta.y);
        if let (Some(Hold::Point(i)), true) = (under, wheel != 0.0 && response.hovered()) {
            let mut points = curve.points().to_vec();
            points[i].q = (points[i].q * 2f32.powf(wheel / 120.0)).clamp(0.3, 12.0);
            self.settings.set_points(&points);
        }
        if let (true, Some(at)) = (response.double_clicked() && under.is_none(), pointer) {
            self.add_point(at);
        }
        if let (true, Some(at)) = (response.secondary_clicked(), pointer) {
            self.menu = (if let Some(Hold::Point(i)) = under { Some(i) } else { None }, at);
        }
        let (on_point, at) = self.menu;
        response.context_menu(|ui| {
            let curve = self.settings.get();
            let mut points = curve.points().to_vec();
            match on_point.filter(|i| *i < points.len()) {
                Some(i) => {
                    let low = points[i].hz < 1000.0;
                    let was = points[i];
                    for (name, kind, q) in [
                        (if low { "Slope at the start" } else { "Slope at the end" }, Some(if low { Kind::LowCut } else { Kind::HighCut }), CUT_Q),
                        ("Shelf", Some(if low { Kind::LowShelf } else { Kind::HighShelf }), POINT_Q),
                        ("Peak", Some(Kind::Peak), POINT_Q),
                        ("Spike", Some(Kind::Peak), SPIKE_Q),
                        ("By its place (shelf at an end, peak between)", None, POINT_Q),
                    ] {
                        if ui.button(name).clicked() {
                            points[i] = Point { kind, q, ..was };
                            self.settings.set_points(&points);
                            ui.close();
                        }
                    }
                    ui.separator();
                    if ui.button("Take the point away").clicked() {
                        points.remove(i);
                        self.settings.set_points(&points);
                        ui.close();
                    }
                }
                None => {
                    if ui.add_enabled(points.len() < POINTS, egui::Button::new("A new point here")).clicked() {
                        self.add_point(at);
                        ui.close();
                    }
                }
            }
        });

        // the curve, and its points on it
        let curve = self.settings.get();
        let (rate, on) = (self.rate(), self.settings.on.load(Ordering::Relaxed));
        let across = (graph.width() / 2.0) as usize;
        let drawn: Vec<Pos2> = (0..=across)
            .map(|step| {
                let x = graph.left() + step as f32 * 2.0;
                pos2(x, y_of(graph, curve.response_db(rate, hz_at(graph, x) as f64)))
            })
            .collect();
        let colour = if on { AMBER } else { Color32::from_gray(120) };
        // between the 0 dB line and the curve it is filled: above the line that much louder, below quieter
        let zero = y_of(graph, 0.0);
        for at in &drawn {
            painter.line_segment([pos2(at.x, zero), *at], Stroke::new(2.0, colour.gamma_multiply(0.22)));
        }
        painter.line(drawn, Stroke::new(2.5, colour));
        let lit = |hold: Hold| self.held == Some(hold) || (self.held.is_none() && under == Some(hold));
        let label = |at: Pos2, text: String| {
            let (side, x) = if at.x > graph.center().x { (egui::Align2::RIGHT_BOTTOM, at.x - 10.0) } else { (egui::Align2::LEFT_BOTTOM, at.x + 10.0) };
            painter.text(pos2(x, (at.y - 8.0).max(graph.top() + 34.0)), side, text, FontId::proportional(12.0), Color32::WHITE);
        };
        // the ten bands: small squares
        for b in 0..BANDS {
            let (at, lit) = (band_handle(graph, &curve, b), lit(Hold::Band(b)));
            let square = Rect::from_center_size(at, egui::Vec2::splat(if lit { 10.0 } else { 7.0 }));
            painter.rect_filled(square, 1.0, if lit { Color32::WHITE } else { colour.gamma_multiply(0.75) });
            if lit {
                label(at, format!("{} band  {:+.1} dB", hz_text(CENTRES[b]), curve.gains[b]));
            }
        }
        // the points: round
        for i in 0..curve.points().len() {
            let (at, lit) = (handle(graph, &curve, i), lit(Hold::Point(i)));
            painter.circle_filled(at, if lit { 7.5 } else { 6.0 }, if lit { Color32::WHITE } else { colour });
            painter.circle_stroke(at, if lit { 7.5 } else { 6.0 }, Stroke::new(1.5, Color32::from_gray(18)));
            if lit {
                label(at, point_text(&curve, i));
            }
        }
        if let Some(notice) = self.notice() {
            let at = pos2(graph.center().x, graph.top() + 44.0);
            let size = painter.text(at, egui::Align2::CENTER_TOP, &notice, FontId::proportional(13.0), Color32::TRANSPARENT);
            painter.rect_filled(size.expand(8.0), 4.0, Color32::from_black_alpha(215));
            painter.text(at, egui::Align2::CENTER_TOP, &notice, FontId::proportional(13.0), Color32::WHITE);
        }
    }

    /// A new point where the pointer is: a peak, or by its place a shelf if it is now the outermost.
    fn add_point(&mut self, at: Pos2) {
        let mut points = self.settings.get().points().to_vec();
        if points.len() < POINTS {
            let db = db_at(self.graph, at.y);
            points.push(Point::new(hz_at(self.graph, at.x), (db * 10.0).round() / 10.0));
            self.settings.set_points(&points);
        }
    }
}

/// The picture as a 24-bit BMP file.
fn write_bmp(path: &str, image: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = image.size;
    let row = (3 * w + 3) / 4 * 4;
    let mut out = Vec::with_capacity(54 + row * h);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0, 0, 0, 0, 54, 0, 0, 0, 40, 0, 0, 0]);
    out.extend_from_slice(&(w as u32).to_le_bytes());
    out.extend_from_slice(&(h as u32).to_le_bytes());
    out.extend_from_slice(&[1, 0, 24, 0, 0, 0, 0, 0]);
    out.extend_from_slice(&((row * h) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 16]);
    for y in (0..h).rev() {
        for p in &image.pixels[y * w..(y + 1) * w] {
            out.extend_from_slice(&[p.b(), p.g(), p.r()]);
        }
        out.resize(out.len() + row - 3 * w, 0);
    }
    std::fs::write(path, out)
}

impl eframe::App for Window {
    /// Trying it out: a point dragged by the pointer's own events (press on it, twelve moves, release),
    /// two seconds before the picture is taken.
    fn raw_input_hook(&mut self, _ctx: &egui::Context, raw: &mut egui::RawInput) {
        let (Some((i, hz, db)), Some((_, seconds))) = (self.drag, &self.shot) else { return };
        let curve = self.settings.get();
        if self.opened.elapsed().as_secs_f32() < seconds - 2.0 || i >= curve.points().len() || self.graph.width() < 10.0 {
            return;
        }
        let to = pos2(x_of(self.graph, hz), y_of(self.graph, db));
        let press = |pos, pressed| egui::Event::PointerButton { pos, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers::NONE };
        match self.drag_step {
            0 => raw.events.push(egui::Event::PointerMoved(handle(self.graph, &curve, i))),
            1 => raw.events.push(press(handle(self.graph, &curve, i), true)),
            2..=13 => {
                let from = handle(self.graph, &curve, i);
                raw.events.push(egui::Event::PointerMoved(from + (to - from) / (14 - self.drag_step) as f32));
            }
            14 => raw.events.push(press(to, false)),
            _ => {
                raw.events.push(egui::Event::PointerGone);
                self.drag = None;
            }
        }
        self.drag_step += 1;
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.spectrum.take(&self.ring);
        self.spectrum.look(ui.input(|i| i.stable_dt).min(0.1));
        self.measured();
        if self.measure_soon && self.opened.elapsed().as_secs_f32() > 1.0 {
            self.measure_soon = false;
            self.measure();
        }
        if self.looked.elapsed().as_secs_f32() > 0.5 {
            self.looked = Instant::now();
            if let Some(running) = &self.running {
                self.delay = running.delay();
            }
            // every two seconds: which device Windows plays to now (asked in a thread of its own, as in `open`)
            self.looks += 1;
            if self.looks % 4 == 0 {
                if let Ok(Ok(now)) = std::thread::spawn(audio::outputs).join() {
                    self.outputs.iter_mut().for_each(|o| o.default = now.iter().any(|n| n.id == o.id && n.default));
                }
            }
        }
        egui::Panel::top("top").show(ui, |ui| {
            ui.add_space(4.0);
            self.top_rows(ui);
            ui.add_space(4.0);
        });
        egui::Panel::bottom("bottom").show(ui, |ui| {
            ui.add_space(4.0);
            self.bottom_rows(ui);
            ui.add_space(2.0);
        });
        egui::CentralPanel::default().show(ui, |ui| self.graph(ui));

        // the picture, when one was asked for
        if let Some((path, seconds)) = &self.shot {
            if !self.shot_asked && self.opened.elapsed().as_secs_f32() > *seconds {
                self.shot_asked = true;
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
            }
            let image = ui.input(|i| i.events.iter().find_map(|e| if let egui::Event::Screenshot { image, .. } = e { Some(image.clone()) } else { None }));
            if let Some(image) = image {
                if let Err(e) = write_bmp(path, &image) {
                    eprintln!("{path}: {e}");
                }
                ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            }
        }
        // the spectrum moves by itself
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(if self.running.is_some() { 16 } else { 100 }));
    }
}

/// Open the window; returns when it is closed (the path stops with it).
pub fn open(start: Start) -> Result<(), String> {
    // the devices are asked for in a thread of their own: this one is the window's, and the window
    // system wants it set up for COM in its own way
    let outputs = std::thread::spawn(audio::outputs).join().map_err(|_| "the devices could not be listed".to_string())??;
    let find = |wanted: &Option<String>| -> Result<Option<usize>, String> {
        let Some(wanted) = wanted.clone() else { return Ok(None) };
        let found = std::thread::spawn(move || audio::find(&wanted)).join().map_err(|_| "the device could not be looked for".to_string())??;
        Ok(outputs.iter().position(|o| o.id == found.id))
    };
    // not said: captured from where Windows plays; on Linux always from Bottle EQ's own device, the first
    let source = if cfg!(windows) { find(&start.from)?.or(outputs.iter().position(|o| o.default)) } else { Some(0) };
    let target = find(&start.to)?.filter(|t| Some(*t) != source);
    if start.settings.get().points().is_empty() {
        start.settings.set_points(&first_points());
    }
    let mode = start.mode.and_then(|name| eq::mode(&name).ok()).map_or(String::new(), |m| m.name);
    // Linux: nothing is taken from anyone by starting, so it starts as soon as it knows where to play
    let begin = (start.exclusive.is_some() || cfg!(target_os = "linux")) && source.is_some() && target.is_some();
    let inputs = std::thread::spawn(audio::inputs).join().map_err(|_| "the microphones could not be listed".to_string())?.unwrap_or_default();
    let wanted = start.mic.as_ref().map(|m| m.to_lowercase());
    let mic = wanted.and_then(|w| inputs.iter().position(|i| i.name.to_lowercase().contains(&w))).or(inputs.iter().position(|i| i.default)).or((!inputs.is_empty()).then_some(0));
    let mut window = Window {
        outputs,
        source,
        target,
        exclusive: start.exclusive.unwrap_or(cfg!(windows)),
        cushion_ms: start.cushion_ms,
        settings: start.settings,
        running: None,
        ring: Arc::new(Ring::new(4 * SIZE)),
        spectrum: Spectrum::new(),
        modes: eq::modes()?,
        mode,
        on_top: true,
        held: None,
        menu: (None, Pos2::ZERO),
        graph: Rect::ZERO,
        looked: Instant::now(),
        looks: 0,
        delay: (0.0, 0.0),
        said: String::new(),
        opened: Instant::now(),
        play: start.play,
        drag: start.drag,
        drag_step: 0,
        shot: start.shot,
        shot_asked: false,
        inputs,
        mic,
        measuring: None,
        measured: None,
        measure_said: String::new(),
        measure_soon: start.measure,
    };
    if begin {
        window.start();
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default().with_title("Bottle EQ").with_inner_size([700.0, 430.0]).with_min_inner_size([520.0, 300.0]).with_always_on_top().with_drag_and_drop(false),
        ..Default::default()
    };
    eframe::run_native("Bottle EQ", options, Box::new(move |made| {
            // dark whatever Windows is set to: the two colours are chosen against it
            made.egui_ctx.set_theme(egui::Theme::Dark);
            Ok(Box::new(window))
        })).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph() -> Rect {
        Rect::from_min_size(pos2(10.0, 50.0), egui::vec2(600.0, 240.0))
    }

    fn curve(points: &[Point]) -> Curve {
        let settings = Settings::default();
        settings.set_points(points);
        settings.get()
    }

    #[test]
    fn the_graph_is_from_20_hz_to_20_khz_and_back() {
        let g = graph();
        assert_eq!(x_of(g, 20.0), g.left());
        assert!((x_of(g, 20000.0) - g.right()).abs() < 0.01);
        // 632 Hz is the middle of 20 and 20,000
        assert!((x_of(g, 632.46) - g.center().x).abs() < 0.1);
        for hz in [31.5, 100.0, 1000.0, 12000.0] {
            assert!((hz_at(g, x_of(g, hz)) / hz - 1.0).abs() < 1e-4);
        }
        for db in [-12.0, -3.5, 0.0, 6.0, 12.0] {
            assert!((db_at(g, y_of(g, db)) - db).abs() < 1e-3);
        }
        // past the edges: held at the edges, and a gain at the furthest a gain goes
        assert_eq!(hz_at(g, g.left() - 50.0), 20.0);
        assert_eq!(db_at(g, g.top() - 50.0), MOST);
    }

    #[test]
    fn the_pointer_takes_the_nearest_point_within_reach() {
        let (g, c) = (graph(), curve(&first_points()));
        assert_eq!(nearest(g, &c, handle(g, &c, 1) + egui::vec2(5.0, -6.0)), Some(Hold::Point(1)));
        assert_eq!(nearest(g, &c, handle(g, &c, 2) + egui::vec2(-9.0, 0.0)), Some(Hold::Point(2)));
        assert_eq!(nearest(g, &c, band_handle(g, &c, 4) + egui::vec2(2.0, 3.0)), Some(Hold::Band(4)));
        assert_eq!(nearest(g, &c, handle(g, &c, 1) + egui::vec2(0.0, 40.0)), None);
        // a point lying on a band is the one taken
        let on_band = curve(&[Point::new(CENTRES[5], 0.0)]);
        assert_eq!(nearest(g, &on_band, band_handle(g, &on_band, 5)), Some(Hold::Point(0)));
    }

    #[test]
    fn a_dragged_point_goes_where_it_is_put() {
        let (g, c) = (graph(), curve(&first_points()));
        // the end raised: the highest point up to +8 dB at 10 kHz, and the curve is raised there
        let points = dragged(g, &c, 2, pos2(x_of(g, 10000.0), y_of(g, 8.0)));
        assert!((points[2].hz / 10000.0 - 1.0).abs() < 1e-3 && (points[2].db - 8.0).abs() < 0.06, "{:?}", points[2]);
        assert_eq!(points[..2], first_points()[..2]);
        let raised = curve(&points);
        assert_eq!(raised.kind(2), Kind::HighShelf);
        assert!(raised.response_db(48000.0, 18000.0) > 7.0 && raised.response_db(48000.0, 200.0).abs() < 0.1);
        // a hair off the 0 dB line is the line
        assert_eq!(dragged(g, &c, 1, pos2(x_of(g, 500.0), y_of(g, 0.2)))[1].db, 0.0);
        // a slope at the start has no gain: only its frequency follows the pointer
        let cut = curve(&[Point { kind: Some(Kind::LowCut), q: CUT_Q, ..Point::new(60.0, 0.0) }, Point::new(1400.0, 0.0)]);
        assert_eq!(handle(g, &cut, 0).y, y_of(g, 0.0));
        let points = dragged(g, &cut, 0, pos2(x_of(g, 120.0), y_of(g, 9.0)));
        assert!((points[0].hz / 120.0 - 1.0).abs() < 1e-3 && points[0].db == 0.0);
        assert!(curve(&points).response_db(48000.0, 30.0) < -20.0);
    }
}
