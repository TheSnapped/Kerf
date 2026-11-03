#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod ffmpeg;
mod playback;

use eframe::egui::{self, pos2, Align2, Color32, FontId, Key, Rect, Sense, Stroke, TextureHandle};
use ffmpeg::{Encoder, ExportJob, ExportSettings, MediaInfo, Mode, Tools, PEAKS_PER_SEC};
use playback::{Playback, PlaybackParams};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;

const PREVIEW_MAX_WIDTH: u32 = 960;
const MIN_VIEW_SECS: f64 = 0.2;

const RED: Color32 = Color32::from_rgb(130, 40, 40);
const WAVE: Color32 = Color32::from_rgb(90, 200, 120);
const WAVE_MUTED: Color32 = Color32::from_rgb(90, 90, 100);

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Kerf")
            .with_inner_size([1100.0, 820.0])
            .with_min_inner_size([700.0, 520.0])
            .with_drag_and_drop(true)
            .with_icon(eframe::icon_data::from_png_bytes(include_bytes!("../assets/kerf.png")).expect("valid icon")),
        ..Default::default()
    };
    eframe::run_native("Kerf", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

// ---------------------------------------------------------------------------
// Background frame grabber for scrubbing: always serves the most recent request only.

struct FrameRequest {
    path: PathBuf,
    time: f64,
    w: u32,
    h: u32,
}

struct FrameResult {
    time: f64,
    w: u32,
    h: u32,
    rgba: Result<Vec<u8>, String>,
}

struct FrameWorker {
    tx: Sender<FrameRequest>,
    rx: Receiver<FrameResult>,
}

impl FrameWorker {
    fn spawn(tools: Tools, ctx: egui::Context) -> Self {
        let (req_tx, req_rx) = channel::<FrameRequest>();
        let (res_tx, res_rx) = channel();
        std::thread::spawn(move || {
            while let Ok(mut req) = req_rx.recv() {
                while let Ok(newer) = req_rx.try_recv() {
                    req = newer;
                }
                let rgba = ffmpeg::grab_frame(&tools, &req.path, req.time, req.w, req.h);
                if res_tx.send(FrameResult { time: req.time, w: req.w, h: req.h, rgba }).is_err() {
                    break;
                }
                ctx.request_repaint();
            }
        });
        Self { tx: req_tx, rx: res_rx }
    }
}

// ---------------------------------------------------------------------------

struct Video {
    path: PathBuf,
    info: MediaInfo,
    time: f64,
    sel_in: Option<f64>,
    sel_out: Option<f64>,
    /// Ranges cut from both video and audio.
    removed: Vec<(f64, f64)>,
    /// Ranges where only the audio is silenced.
    muted: Vec<(f64, f64)>,
    texture: Option<TextureHandle>,
    requested: Option<f64>,
    waveform: Option<Vec<f32>>,
    /// Visible part of the timeline.
    view_start: f64,
    view_len: f64,
    /// Set by user seeks so a running playback restarts from the new spot.
    seeked: bool,
    /// Bumped on every edit so a running playback picks up new ranges.
    edits: u64,
}

impl Video {
    fn preview_size(&self) -> (u32, u32) {
        let w = self.info.width.min(PREVIEW_MAX_WIDTH);
        let h = (self.info.height as f64 * w as f64 / self.info.width as f64).round() as u32;
        (w & !1, (h & !1).max(2))
    }

    /// Ranges of the source that survive, with removals merged.
    fn keep_ranges(&self) -> Vec<(f64, f64)> {
        let mut removed = self.removed.clone();
        removed.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut keep = Vec::new();
        let mut cursor = 0.0;
        for (a, b) in removed {
            if a > cursor {
                keep.push((cursor, a));
            }
            cursor = f64::max(cursor, b);
        }
        if cursor < self.info.duration {
            keep.push((cursor, self.info.duration));
        }
        keep.retain(|(a, b)| b - a > 0.05);
        keep
    }

    fn selection(&self) -> Option<(f64, f64)> {
        let (a, b) = (self.sel_in?, self.sel_out?);
        (b > a).then_some((a, b))
    }

    fn seek(&mut self, t: f64) {
        self.time = t.clamp(0.0, self.info.duration);
        self.seeked = true;
    }

    fn removed_at(&self, t: f64) -> Option<(f64, f64)> {
        self.removed.iter().copied().find(|&(a, b)| t >= a && t < b)
    }

    fn clamp_view(&mut self) {
        let dur = self.info.duration;
        self.view_len = self.view_len.clamp(MIN_VIEW_SECS.min(dur), dur);
        self.view_start = self.view_start.clamp(0.0, dur - self.view_len);
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Resolution {
    Original,
    P1080,
    P720,
    P480,
}

impl Resolution {
    const ALL: [Self; 4] = [Self::Original, Self::P1080, Self::P720, Self::P480];

    fn label(self) -> &'static str {
        match self {
            Self::Original => "Original",
            Self::P1080 => "1080p",
            Self::P720 => "720p",
            Self::P480 => "480p",
        }
    }

    fn max_height(self) -> Option<u32> {
        match self {
            Self::Original => None,
            Self::P1080 => Some(1080),
            Self::P720 => Some(720),
            Self::P480 => Some(480),
        }
    }
}

struct App {
    tools: Result<Tools, String>,
    worker: Option<FrameWorker>,
    video: Option<Video>,
    error: Option<String>,
    waveform_rx: Option<Receiver<(PathBuf, Vec<f32>)>>,

    playback: Option<Playback>,
    playback_edits: u64,
    /// Playback was paused by dragging the playhead and resumes on release.
    resume_after_scrub: bool,
    mute_flag: Arc<AtomicBool>,

    mute: bool,
    fast: bool,
    resolution: Resolution,
    use_target_size: bool,
    target_mb: f64,
    encoder: Encoder,
    /// Filled in by a background probe at startup.
    nvenc: Option<bool>,
    nvenc_rx: Option<Receiver<bool>>,

    job: Option<ExportJob>,
    job_output: Option<PathBuf>,
    last_result: Option<Result<String, String>>,
}

impl App {
    fn new(cc: &eframe::CreationContext) -> Self {
        let mut app = Self {
            tools: Err(String::new()),
            worker: None,
            video: None,
            error: None,
            waveform_rx: None,
            playback: None,
            playback_edits: 0,
            resume_after_scrub: false,
            mute_flag: Arc::default(),
            mute: false,
            fast: false,
            resolution: Resolution::Original,
            use_target_size: false,
            target_mb: 25.0,
            encoder: Encoder::X264,
            nvenc: None,
            nvenc_rx: None,
            job: None,
            job_output: None,
            last_result: None,
        };
        app.init_tools(&cc.egui_ctx);
        // Supports "Open with…", dropping a file onto the exe, and `kerf <file> --at <seconds>`
        // (used by the Glance player's "Open in Kerf" button).
        let mut args = std::env::args_os().skip(1);
        let (mut path, mut at) = (None, None);
        while let Some(arg) = args.next() {
            if arg == "--at" {
                at = args.next().and_then(|t| t.to_str()?.parse::<f64>().ok());
            } else {
                path.get_or_insert(PathBuf::from(arg));
            }
        }
        if let Some(path) = path {
            app.open(path, &cc.egui_ctx);
            if let (Some(v), Some(t)) = (&mut app.video, at) {
                v.seek(t);
            }
        }
        app
    }

    /// Finds ffmpeg/ffprobe and starts the helpers that need them. Called again by
    /// "Check again" after the user installs ffmpeg.
    fn init_tools(&mut self, ctx: &egui::Context) {
        self.tools = Tools::locate();
        let Ok(tools) = &self.tools else { return };
        self.worker = Some(FrameWorker::spawn(tools.clone(), ctx.clone()));
        let (tx, rx) = channel();
        let (t, ctx) = (tools.clone(), ctx.clone());
        std::thread::spawn(move || {
            let _ = tx.send(ffmpeg::nvenc_available(&t));
            ctx.request_repaint();
        });
        self.nvenc_rx = Some(rx);
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        let Ok(tools) = &self.tools else { return };
        self.playback = None;
        match ffmpeg::probe(tools, &path) {
            Ok(info) => {
                if info.has_audio {
                    let (tx, rx) = channel();
                    let (tools, path, ctx) = (tools.clone(), path.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let peaks = ffmpeg::waveform(&tools, &path).unwrap_or_default();
                        let _ = tx.send((path, peaks));
                        ctx.request_repaint();
                    });
                    self.waveform_rx = Some(rx);
                }
                self.video = Some(Video {
                    path,
                    view_len: info.duration,
                    info,
                    time: 0.0,
                    sel_in: None,
                    sel_out: None,
                    removed: Vec::new(),
                    muted: Vec::new(),
                    texture: None,
                    requested: None,
                    waveform: None,
                    view_start: 0.0,
                    seeked: false,
                    edits: 0,
                });
                self.error = None;
                self.last_result = None;
            }
            Err(e) => self.error = Some(format!("Could not open {}: {e}", path.display())),
        }
    }

    fn pick_file(&mut self, ctx: &egui::Context) {
        if let Some(path) = rfd::FileDialog::new()
            .add_filter("Video", &["mp4", "mkv", "mov", "avi", "webm", "m4v", "wmv", "flv", "ts"])
            .add_filter("All files", &["*"])
            .pick_file()
        {
            self.open(path, ctx);
        }
    }

    fn poll_waveform(&mut self) {
        let (Some(rx), Some(v)) = (&self.waveform_rx, &mut self.video) else { return };
        if let Ok((path, peaks)) = rx.try_recv() {
            if path == v.path {
                v.waveform = Some(peaks);
            }
            self.waveform_rx = None;
        }
    }

    fn poll_frames(&mut self, ctx: &egui::Context) {
        let (Some(worker), Some(video)) = (&self.worker, &mut self.video) else { return };
        while let Ok(res) = worker.rx.try_recv() {
            // Drop stale results: another file, or playback has taken over the texture.
            if (res.w, res.h) != video.preview_size() || self.playback.is_some() {
                continue;
            }
            match res.rgba {
                Ok(rgba) => set_frame(ctx, video, res.w, res.h, &rgba),
                // Seeking past the last frame yields nothing; keep the old frame.
                Err(_) if res.time > 0.0 => {}
                Err(e) => self.error = Some(format!("Preview failed: {e}")),
            }
        }
        if self.playback.is_none() && video.requested != Some(video.time) {
            let (w, h) = video.preview_size();
            let _ = worker.tx.send(FrameRequest { path: video.path.clone(), time: video.time, w, h });
            video.requested = Some(video.time);
        }
    }

    fn toggle_playback(&mut self, ctx: &egui::Context) {
        if self.playback.is_some() {
            self.playback = None;
            return;
        }
        let Some(v) = &mut self.video else { return };
        if v.time >= v.info.duration - 0.05 {
            v.time = 0.0;
        }
        self.start_playback(ctx);
    }

    fn start_playback(&mut self, ctx: &egui::Context) {
        let (Ok(tools), Some(v)) = (&self.tools, &mut self.video) else { return };
        self.playback = None;
        // Never start inside a removed part.
        while let Some((_, end)) = v.removed_at(v.time) {
            v.time = end;
        }
        if v.time >= v.info.duration - 0.02 {
            return;
        }
        let (width, height) = v.preview_size();
        let params = PlaybackParams {
            path: v.path.clone(),
            start: v.time,
            fps: v.info.fps,
            width,
            height,
            has_audio: v.info.has_audio,
            silent: v.removed.iter().chain(&v.muted).copied().collect(),
            mute: self.mute_flag.clone(),
        };
        let ctx = ctx.clone();
        match Playback::start(tools, params, move || ctx.request_repaint()) {
            Ok(pb) => {
                self.playback = Some(pb);
                self.playback_edits = v.edits;
            }
            Err(e) => self.error = Some(format!("Playback failed: {e}")),
        }
    }

    fn update_playback(&mut self, ctx: &egui::Context) {
        self.mute_flag.store(self.mute, Ordering::Relaxed);
        let Some(v) = &mut self.video else { return };
        let seeked = std::mem::take(&mut v.seeked);
        if self.playback.is_none() {
            return;
        }
        if seeked || v.edits != self.playback_edits {
            self.start_playback(ctx);
        }
        let (Some(pb), Some(v)) = (&mut self.playback, &mut self.video) else { return };

        if let Some(rgba) = pb.next_frame() {
            let (w, h) = v.preview_size();
            set_frame(ctx, v, w, h, &rgba);
        }
        v.time = pb.time().min(v.info.duration);
        v.requested = Some(v.time);
        // Keep the playhead on screen.
        if v.time > v.view_start + v.view_len || v.time < v.view_start {
            v.view_start = v.time;
            v.clamp_view();
        }

        let skip_to = v.removed_at(v.time).map(|(_, end)| end);
        if pb.finished() || v.time >= v.info.duration - 0.01 {
            self.playback = None;
        } else if let Some(end) = skip_to {
            v.time = end;
            self.start_playback(ctx);
        }
        ctx.request_repaint();
    }

    fn handle_keys(&mut self, ctx: &egui::Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let Some(v) = &mut self.video else { return };
        let mut toggle = false;
        ctx.input(|i| {
            let step = if i.modifiers.shift { 1.0 } else { 0.1 };
            if i.key_pressed(Key::Space) {
                toggle = true;
            }
            if i.key_pressed(Key::ArrowLeft) {
                v.seek(v.time - step);
            }
            if i.key_pressed(Key::ArrowRight) {
                v.seek(v.time + step);
            }
            if i.key_pressed(Key::Home) {
                v.seek(0.0);
            }
            if i.key_pressed(Key::End) {
                v.seek(v.info.duration);
            }
            if i.key_pressed(Key::I) {
                v.sel_in = Some(v.time);
            }
            if i.key_pressed(Key::O) {
                v.sel_out = Some(v.time);
            }
            if i.key_pressed(Key::Delete) || i.key_pressed(Key::Backspace) {
                cut_selection(v, false);
            }
            if i.key_pressed(Key::M) {
                cut_selection(v, true);
            }
            if i.key_pressed(Key::Escape) {
                v.sel_in = None;
                v.sel_out = None;
            }
        });
        if toggle {
            self.toggle_playback(ctx);
        }
    }

    fn start_export(&mut self, ctx: &egui::Context) {
        self.playback = None;
        let (Ok(tools), Some(v)) = (&self.tools, &self.video) else { return };
        let stem = v.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or("video".into());
        let ext = if self.fast {
            v.path.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into())
        } else {
            "mp4".into()
        };
        let mut dialog = rfd::FileDialog::new()
            .set_file_name(format!("{stem}_edited.{ext}"))
            .add_filter(ext.to_uppercase(), &[ext.as_str()]);
        if let Some(dir) = v.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        let Some(output) = dialog.save_file() else { return };
        if output == v.path {
            self.last_result = Some(Err("Choose a different file than the source.".into()));
            return;
        }

        let settings = ExportSettings {
            input: v.path.clone(),
            output: output.clone(),
            keep: v.keep_ranges(),
            keep_audio: v.info.has_audio && !self.mute,
            muted_audio: v.muted.clone(),
            mode: if self.fast {
                Mode::Fast
            } else {
                Mode::Reencode {
                    max_height: self.resolution.max_height(),
                    target_mb: self.use_target_size.then_some(self.target_mb),
                    encoder: self.encoder,
                }
            },
        };
        let ctx = ctx.clone();
        self.job = Some(ExportJob::start(tools.clone(), settings, move || ctx.request_repaint()));
        self.job_output = Some(output);
        self.last_result = None;
    }

    fn poll_job(&mut self) {
        let Some(job) = &self.job else { return };
        let p = job.progress.lock().unwrap().clone();
        if !p.finished {
            return;
        }
        self.last_result = Some(match p.error {
            None => {
                let path = self.job_output.take().unwrap_or_default();
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                Ok(format!("Saved {} ({})", path.display(), format_bytes(size)))
            }
            Some(e) => Err(e),
        });
        self.job = None;
    }
}

fn set_frame(ctx: &egui::Context, v: &mut Video, w: u32, h: u32, rgba: &[u8]) {
    let img = egui::ColorImage::from_rgba_unmultiplied([w as usize, h as usize], rgba);
    match &mut v.texture {
        Some(t) => t.set(img, egui::TextureOptions::LINEAR),
        None => v.texture = Some(ctx.load_texture("frame", img, egui::TextureOptions::LINEAR)),
    }
}

/// Moves the selection into the removed list, or into the audio-muted list.
fn cut_selection(v: &mut Video, audio_only: bool) {
    if let Some(sel) = v.selection() {
        let list = if audio_only { &mut v.muted } else { &mut v.removed };
        list.push(sel);
        list.sort_by(|a, b| a.0.total_cmp(&b.0));
        v.sel_in = None;
        v.sel_out = None;
        v.edits += 1;
    }
}

fn format_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0).floor() as u64;
    let s = t - m as f64 * 60.0;
    format!("{m:02}:{s:05.2}")
}

fn format_bytes(b: u64) -> String {
    let mb = b as f64 / 1_000_000.0;
    if mb >= 1.0 { format!("{mb:.1} MB") } else { format!("{:.0} KB", b as f64 / 1000.0) }
}

/// Shows the ranges in `list` as clickable chips; ✖ restores one.
fn range_chips(ui: &mut egui::Ui, label: &str, list: &mut Vec<(f64, f64)>, time: &mut f64, edits: &mut u64, seeked: &mut bool) {
    if list.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(label);
        let mut delete = None;
        for (i, &(a, b)) in list.iter().enumerate() {
            ui.group(|ui| {
                if ui.link(format!("{} – {}", format_time(a), format_time(b))).clicked() {
                    *time = a;
                    *seeked = true;
                }
                if ui.small_button("✖").on_hover_text("Restore").clicked() {
                    delete = Some(i);
                }
            });
        }
        if let Some(i) = delete {
            list.remove(i);
            *edits += 1;
        }
    });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let dropped = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            if self.job.is_none() {
                self.open(path, ctx);
            }
        }
        self.poll_job();
        if let Some(found) = self.nvenc_rx.as_ref().and_then(|rx| rx.try_recv().ok()) {
            // Default to the GPU when there is one; it is several times faster.
            if found {
                self.encoder = Encoder::Nvenc;
            }
            self.nvenc = Some(found);
            self.nvenc_rx = None;
        }
        self.poll_waveform();
        self.handle_keys(ctx);
        self.update_playback(ctx);
        self.poll_frames(ctx);
        let busy = self.job.is_some();

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui.add_enabled(!busy && self.tools.is_ok(), egui::Button::new("📂 Open video…")).clicked() {
                    self.pick_file(ctx);
                }
                if let Some(v) = &self.video {
                    ui.strong(v.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default());
                    ui.label(format!(
                        "{}×{} · {:.2} fps · {} · {}",
                        v.info.width,
                        v.info.height,
                        v.info.fps,
                        format_time(v.info.duration),
                        if v.info.has_audio { "audio" } else { "no audio" }
                    ));
                } else {
                    ui.weak("…or drop a video file onto the window");
                }
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("export").show(ctx, |ui| {
            ui.add_space(6.0);
            self.export_panel(ui, ctx, busy);
            ui.add_space(6.0);
        });

        if self.video.is_some() {
            egui::TopBottomPanel::bottom("timeline").show(ctx, |ui| {
                ui.add_space(6.0);
                self.timeline_panel(ui, ctx, busy);
                ui.add_space(6.0);
            });
        }

        egui::CentralPanel::default().show(ctx, |ui| {
            if let Err(e) = &self.tools {
                let mut retry = false;
                ui.vertical_centered(|ui| {
                    ui.add_space(ui.available_height() / 3.0);
                    ui.heading("Kerf needs ffmpeg");
                    ui.colored_label(Color32::LIGHT_RED, format!("{e}."));
                    ui.add_space(8.0);
                    ui.label("Install it from a terminal:");
                    ui.code("winget install Gyan.FFmpeg");
                    ui.label("or put ffmpeg.exe and ffprobe.exe next to kerf.exe.");
                    ui.add_space(8.0);
                    retry = ui.button("Check again").clicked();
                });
                if retry {
                    self.init_tools(ctx);
                }
                return;
            }
            if let Some(err) = &self.error {
                ui.colored_label(Color32::LIGHT_RED, err);
            }
            match self.video.as_ref().and_then(|v| v.texture.as_ref()) {
                Some(tex) => {
                    let avail = ui.available_size();
                    let ts = tex.size_vec2();
                    let scale = (avail.x / ts.x).min(avail.y / ts.y);
                    ui.centered_and_justified(|ui| {
                        ui.add(egui::Image::new(tex).fit_to_exact_size(ts * scale));
                    });
                }
                None if self.video.is_some() => {
                    ui.centered_and_justified(|ui| ui.spinner());
                }
                None => {
                    ui.centered_and_justified(|ui| ui.heading("Open a video to start"));
                }
            }
        });
    }
}

impl App {
    fn timeline_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, busy: bool) {
        let playing = self.playback.is_some();
        let mut toggle = false;
        let mut resume = false;
        let Some(v) = &mut self.video else { return };
        let dur = v.info.duration;

        // Transport and editing controls
        ui.add_enabled_ui(!busy, |ui| {
            ui.horizontal(|ui| {
                if ui.button(if playing { "⏸ Pause" } else { "▶ Play" }).on_hover_text("Space").clicked() {
                    toggle = true;
                }
                ui.monospace(format!("{} / {}", format_time(v.time), format_time(dur)));
                ui.separator();
                for (label, d) in [("⏪ 1s", -1.0), ("◀ 0.1s", -0.1), ("0.1s ▶", 0.1), ("1s ⏩", 1.0)] {
                    if ui.button(label).clicked() {
                        v.seek(v.time + d);
                    }
                }
                ui.separator();
                if ui.button("Set In [I]").clicked() {
                    v.sel_in = Some(v.time);
                }
                if ui.button("Set Out [O]").clicked() {
                    v.sel_out = Some(v.time);
                }
                let has_sel = v.selection().is_some();
                if ui.add_enabled(has_sel, egui::Button::new("✂ Remove [Del]")).on_hover_text("Cut video and audio").clicked() {
                    cut_selection(v, false);
                }
                if ui.add_enabled(has_sel && v.info.has_audio, egui::Button::new("🔇 Mute audio [M]")).on_hover_text("Silence the audio in the selection, keep the video").clicked() {
                    cut_selection(v, true);
                }
                if (v.sel_in.is_some() || v.sel_out.is_some()) && ui.button("Clear [Esc]").clicked() {
                    v.sel_in = None;
                    v.sel_out = None;
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Fit").on_hover_text("Show the whole video").clicked() {
                        v.view_start = 0.0;
                        v.view_len = dur;
                    }
                    ui.weak("scroll: zoom · shift+scroll: pan");
                });
            });
        });
        ui.add_space(4.0);

        // Ruler + video track + audio track share one interactive area.
        let ruler_h = 18.0;
        let video_h = 30.0;
        let audio_h = if v.info.has_audio { 64.0 } else { 0.0 };
        let (rect, resp) = ui.allocate_exact_size(
            egui::vec2(ui.available_width(), ruler_h + video_h + audio_h + 6.0),
            Sense::click_and_drag(),
        );
        let ruler = Rect::from_min_size(rect.min, egui::vec2(rect.width(), ruler_h));
        let vtrack = Rect::from_min_size(pos2(rect.left(), ruler.bottom() + 2.0), egui::vec2(rect.width(), video_h));
        let atrack = Rect::from_min_size(pos2(rect.left(), vtrack.bottom() + 4.0), egui::vec2(rect.width(), audio_h));

        // Zoom around the pointer / pan.
        if resp.hovered() {
            let (scroll, pointer) = ui.input(|i| (i.smooth_scroll_delta, i.pointer.hover_pos()));
            if let Some(p) = pointer {
                if scroll.y != 0.0 {
                    let anchor = v.view_start + ((p.x - rect.left()) / rect.width()) as f64 * v.view_len;
                    let new_len = (v.view_len * (-(scroll.y as f64) * 0.004).exp()).clamp(MIN_VIEW_SECS.min(dur), dur);
                    v.view_start = anchor - (anchor - v.view_start) * new_len / v.view_len;
                    v.view_len = new_len;
                }
                if scroll.x != 0.0 {
                    v.view_start -= scroll.x as f64 / rect.width() as f64 * v.view_len;
                }
                v.clamp_view();
            }
        }
        let (vs, vl) = (v.view_start, v.view_len);
        let x_of = |t: f64| rect.left() + ((t - vs) / vl) as f32 * rect.width();
        let t_of = |x: f32| (vs + ((x - rect.left()) / rect.width()) as f64 * vl).clamp(0.0, dur);

        if resp.drag_started() && playing {
            self.playback = None;
            self.resume_after_scrub = true;
        }
        if resp.drag_stopped() && std::mem::take(&mut self.resume_after_scrub) {
            resume = true;
        }
        if !busy {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = t_of(pos.x);
                if (t - v.time).abs() > 1e-6 {
                    v.seek(t);
                }
            }
        }

        let painter = ui.painter_at(rect);
        let span = |a: f64, b: f64, r: Rect| Rect::from_x_y_ranges(x_of(a)..=x_of(b), r.y_range());

        // Ruler with ticks at a "nice" interval of at least ~80 px.
        let px_per_sec = rect.width() as f64 / vl;
        let step = [0.1, 0.2, 0.5, 1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]
            .into_iter()
            .find(|s| s * px_per_sec >= 80.0)
            .unwrap_or(7200.0);
        let text_color = ui.visuals().weak_text_color();
        let mut t = (vs / step).ceil() * step;
        while t <= vs + vl {
            let x = x_of(t);
            painter.vline(x, ruler.bottom() - 5.0..=ruler.bottom(), Stroke::new(1.0_f32, text_color));
            painter.text(pos2(x + 3.0, ruler.top()), Align2::LEFT_TOP, format_time(t), FontId::monospace(10.0), text_color);
            t += step;
        }

        // Video track
        painter.rect_filled(vtrack, 3.0, Color32::from_rgb(45, 60, 85));
        painter.text(vtrack.left_center() + egui::vec2(6.0, 0.0), Align2::LEFT_CENTER, "Video", FontId::proportional(11.0), Color32::from_gray(170));

        // Audio track with waveform
        if v.info.has_audio {
            painter.rect_filled(atrack, 3.0, Color32::from_rgb(30, 45, 35));
            match &v.waveform {
                Some(peaks) if !peaks.is_empty() => {
                    let norm = peaks.iter().cloned().fold(0.01_f32, f32::max);
                    let mid = atrack.center().y;
                    let half = atrack.height() / 2.0 - 2.0;
                    let x0 = x_of(0.0).max(rect.left()) as i32;
                    let x1 = x_of(dur).min(rect.right()) as i32;
                    for x in x0..x1 {
                        let (ta, tb) = (t_of(x as f32), t_of(x as f32 + 1.0));
                        let ia = (ta * PEAKS_PER_SEC) as usize;
                        let ib = ((tb * PEAKS_PER_SEC).ceil() as usize).clamp(ia + 1, peaks.len().max(ia + 1));
                        let peak = peaks.get(ia..ib.min(peaks.len())).map_or(0.0, |s| s.iter().cloned().fold(0.0, f32::max)) / norm;
                        let tm = (ta + tb) / 2.0;
                        let silent = v.muted.iter().chain(&v.removed).any(|&(a, b)| tm >= a && tm < b);
                        let h = (peak * half).max(0.5);
                        painter.vline(x as f32 + 0.5, mid - h..=mid + h, Stroke::new(1.0_f32, if silent { WAVE_MUTED } else { WAVE }));
                    }
                }
                _ => {
                    painter.text(atrack.center(), Align2::CENTER_CENTER, "analyzing audio…", FontId::proportional(11.0), text_color);
                }
            }
            for &(a, b) in &v.muted {
                let r = span(a, b, atrack);
                painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(0, 0, 0, 110));
                painter.rect_stroke(r, 0.0, Stroke::new(1.0_f32, Color32::from_rgb(200, 160, 60)), egui::StrokeKind::Inside);
            }
            painter.text(atrack.left_top() + egui::vec2(6.0, 3.0), Align2::LEFT_TOP, "Audio", FontId::proportional(11.0), Color32::from_gray(170));
        }

        // Removed ranges cover both tracks.
        let tracks = Rect::from_min_max(vtrack.min, if v.info.has_audio { atrack.max } else { vtrack.max });
        for &(a, b) in &v.removed {
            let r = span(a, b, tracks);
            painter.rect_filled(r, 0.0, Color32::from_rgba_unmultiplied(RED.r(), RED.g(), RED.b(), 200));
            painter.line_segment([r.left_top(), r.right_bottom()], Stroke::new(1.0_f32, Color32::from_rgb(190, 80, 80)));
        }
        if let Some((a, b)) = v.selection() {
            painter.rect_filled(span(a, b, tracks), 0.0, Color32::from_rgba_unmultiplied(80, 140, 255, 70));
        }
        for (mark, color) in [(v.sel_in, Color32::from_rgb(80, 200, 255)), (v.sel_out, Color32::from_rgb(255, 180, 60))] {
            if let Some(t) = mark {
                painter.vline(x_of(t), tracks.y_range(), Stroke::new(2.0_f32, color));
            }
        }
        let px = x_of(v.time);
        painter.vline(px, ruler.top()..=tracks.bottom(), Stroke::new(2.0_f32, Color32::WHITE));
        painter.circle_filled(pos2(px, ruler.top() + 3.0), 4.0, Color32::WHITE);

        // Zoomed-in overview: a thin bar showing which part of the video is visible.
        if vl < dur {
            let (bar, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 4.0), Sense::hover());
            let p = ui.painter();
            p.rect_filled(bar, 2.0, Color32::from_gray(50));
            let a = bar.left() + (vs / dur) as f32 * bar.width();
            let b = bar.left() + ((vs + vl) / dur) as f32 * bar.width();
            p.rect_filled(Rect::from_x_y_ranges(a..=b, bar.y_range()), 2.0, Color32::from_gray(140));
        }

        ui.add_space(4.0);
        if let (Some(a), Some(b)) = (v.sel_in, v.sel_out) {
            if b <= a {
                ui.colored_label(Color32::YELLOW, "Out point must be after the in point.");
            }
        }
        ui.add_enabled_ui(!busy, |ui| {
            range_chips(ui, "Removed:", &mut v.removed, &mut v.time, &mut v.edits, &mut v.seeked);
            range_chips(ui, "Audio muted:", &mut v.muted, &mut v.time, &mut v.edits, &mut v.seeked);
        });

        if toggle {
            self.toggle_playback(ctx);
        } else if resume {
            self.start_playback(ctx);
        }
    }

    fn export_panel(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, busy: bool) {
        let has_video = self.video.is_some();
        let has_audio = self.video.as_ref().is_some_and(|v| v.info.has_audio);
        let out_len: f64 = self.video.as_ref().map(|v| v.keep_ranges().iter().map(|(a, b)| b - a).sum()).unwrap_or(0.0);
        let partial_mute = self.video.as_ref().is_some_and(|v| !v.muted.is_empty());

        ui.add_enabled_ui(!busy && has_video, |ui| {
            ui.horizontal(|ui| {
                ui.add_enabled(has_audio, egui::Checkbox::new(&mut self.mute, "🔇 Mute all audio"));
                ui.separator();
                ui.radio_value(&mut self.fast, true, "Fast (no re-encode)").on_hover_text(
                    "Instant and lossless video, but cuts snap to the nearest keyframe and no compression is applied.\n\
                     Audio is re-encoded if parts of it are muted.",
                );
                ui.radio_value(&mut self.fast, false, "Compress (re-encode H.264)");
                if self.fast && partial_mute && !self.mute {
                    ui.weak("(audio re-encoded for muted parts)");
                }
            });
            if !self.fast {
                ui.horizontal(|ui| {
                    ui.label("Encoder:");
                    ui.radio_value(&mut self.encoder, Encoder::X264, "CPU (x264)")
                        .on_hover_text("Smallest files for a given quality; slower.");
                    let gpu = ui.add_enabled(
                        self.nvenc == Some(true),
                        egui::RadioButton::new(self.encoder == Encoder::Nvenc, "GPU (NVENC)"),
                    );
                    if gpu.clicked() {
                        self.encoder = Encoder::Nvenc;
                    }
                    gpu.on_hover_text("NVIDIA hardware encoder: much faster, slightly larger files at equal quality.")
                        .on_disabled_hover_text(match self.nvenc {
                            None => "Checking for an NVIDIA GPU…",
                            _ => "No working NVIDIA encoder found.",
                        });
                });
                ui.horizontal(|ui| {
                    egui::ComboBox::from_label("Resolution")
                        .selected_text(self.resolution.label())
                        .show_ui(ui, |ui| {
                            for r in Resolution::ALL {
                                ui.selectable_value(&mut self.resolution, r, r.label());
                            }
                        });
                    ui.separator();
                    ui.checkbox(&mut self.use_target_size, "Target file size");
                    ui.add_enabled(
                        self.use_target_size,
                        egui::DragValue::new(&mut self.target_mb).range(1.0..=10_000.0).speed(0.5).suffix(" MB"),
                    );
                    if self.use_target_size && out_len > 0.0 {
                        let kbps = ffmpeg::video_kbps_for_target(self.target_mb, out_len, has_audio && !self.mute);
                        if kbps < 50.0 {
                            ui.colored_label(Color32::LIGHT_RED, "too small for this length");
                        } else {
                            ui.weak(format!("≈ {:.0} kbps video", kbps));
                        }
                    } else if !self.use_target_size {
                        ui.weak("(quality-based, CRF 23)");
                    }
                });
            }
        });

        ui.add_space(4.0);
        ui.horizontal(|ui| {
            if let Some(job) = &self.job {
                let p = job.progress.lock().unwrap().clone();
                if ui.button("Cancel").clicked() {
                    job.cancel();
                }
                ui.add(egui::ProgressBar::new(p.fraction).text(format!("{} {:.0}%", p.stage, p.fraction * 100.0)));
            } else {
                let can_export = has_video && out_len > 0.0;
                if ui.add_enabled(can_export, egui::Button::new("💾 Export…")).clicked() {
                    self.start_export(ctx);
                }
                if has_video {
                    ui.label(format!("Output length: {}", format_time(out_len)));
                }
                match &self.last_result {
                    Some(Ok(msg)) => {
                        ui.colored_label(Color32::LIGHT_GREEN, msg);
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::LIGHT_RED, e);
                    }
                    None => {}
                }
            }
        });
    }
}
