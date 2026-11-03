//! Thin wrapper around the ffmpeg / ffprobe executables.

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
}

impl Tools {
    /// Looks next to the running executable first, then on PATH.
    pub fn locate() -> Result<Self, String> {
        let ffmpeg = find_tool("ffmpeg").ok_or("ffmpeg not found")?;
        let ffprobe = find_tool("ffprobe").ok_or("ffprobe not found")?;
        Ok(Self { ffmpeg, ffprobe })
    }
}

fn find_tool(name: &str) -> Option<PathBuf> {
    let file = if cfg!(windows) { format!("{name}.exe") } else { name.to_string() };
    let beside_exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join(&file)));
    let on_path = std::env::var_os("PATH")
        .map(|p| std::env::split_paths(&p).map(|d| d.join(&file)).collect::<Vec<_>>())
        .unwrap_or_default();
    // winget's alias folder: lets a fresh `winget install` work without restarting Kerf
    // (a running process keeps its old PATH).
    let winget = std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("Microsoft").join("WinGet").join("Links").join(&file));
    beside_exe.into_iter().chain(on_path).chain(winget).find(|p| p.is_file())
}

/// Builds a command that won't flash a console window on Windows.
pub(crate) fn command(exe: &Path) -> Command {
    let mut c = Command::new(exe);
    c.stdin(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

#[derive(Clone, Debug)]
pub struct MediaInfo {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    pub fps: f64,
}

pub fn probe(tools: &Tools, path: &Path) -> Result<MediaInfo, String> {
    let out = command(&tools.ffprobe)
        .args(["-v", "error", "-show_entries", "format=duration:stream=codec_type,width,height,avg_frame_rate:stream_side_data=rotation"])
        .args(["-of", "default=noprint_wrappers=1"])
        .arg(path)
        .output()
        .map_err(|e| format!("failed to run ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }

    let mut info = MediaInfo { duration: 0.0, width: 0, height: 0, has_audio: false, fps: 0.0 };
    let mut rotation = None;
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let Some((key, value)) = line.split_once('=') else { continue };
        match key {
            "duration" => info.duration = value.parse().unwrap_or(info.duration),
            // Only the first video stream's size matters.
            "width" if info.width == 0 => info.width = value.parse().unwrap_or(0),
            "height" if info.height == 0 => info.height = value.parse().unwrap_or(0),
            "codec_type" if value == "audio" => info.has_audio = true,
            // Audio streams report "0/0", which parse_rate rejects.
            "avg_frame_rate" if info.fps == 0.0 => info.fps = parse_rate(value).unwrap_or(0.0),
            "rotation" if rotation.is_none() => rotation = value.parse::<f64>().ok(),
            _ => {}
        }
    }
    if info.fps <= 0.0 {
        info.fps = 30.0;
    }
    // Phone videos store portrait as landscape + rotation; ffmpeg rotates frames on
    // decode, so report the size as displayed.
    if rotation.is_some_and(|r| (r.rem_euclid(180.0) - 90.0).abs() < 1.0) {
        std::mem::swap(&mut info.width, &mut info.height);
    }
    if info.width == 0 || info.height == 0 {
        return Err("no video stream found".into());
    }
    if info.duration <= 0.0 {
        return Err("could not determine duration".into());
    }
    Ok(info)
}

fn parse_rate(s: &str) -> Option<f64> {
    let (n, d) = s.split_once('/')?;
    let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
    (n > 0.0 && d > 0.0).then(|| n / d)
}

pub const PEAKS_PER_SEC: f64 = 100.0;

/// Peak amplitude (0..1) of the mixed-down audio, `PEAKS_PER_SEC` values per second.
pub fn waveform(tools: &Tools, path: &Path) -> Result<Vec<f32>, String> {
    const RATE: f64 = 8000.0;
    let mut child = command(&tools.ffmpeg)
        .args(["-v", "error", "-i"])
        .arg(path)
        .args(["-vn", "-ac", "1", "-ar", &RATE.to_string(), "-f", "s16le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut chunk = vec![0u8; (RATE / PEAKS_PER_SEC) as usize * 2];
    let mut peaks = Vec::new();
    while reader.read_exact(&mut chunk).is_ok() {
        let peak = chunk
            .as_chunks::<2>().0.iter()
            .map(|b| i16::from_le_bytes([b[0], b[1]]).unsigned_abs())
            .max()
            .unwrap_or(0);
        peaks.push(peak as f32 / 32768.0);
    }
    let _ = child.wait();
    Ok(peaks)
}

/// Audio encoder that the output container accepts (WebM/Ogg can't hold AAC).
fn audio_codec_for(output: &Path) -> &'static str {
    let ext = output.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "webm" | "ogg" | "ogv" => "libopus",
        _ => "aac",
    }
}

/// The parts of `ranges` inside [start, end], shifted so `start` becomes 0.
fn local_ranges(ranges: &[(f64, f64)], start: f64, end: f64) -> Vec<(f64, f64)> {
    ranges
        .iter()
        .filter(|(a, b)| *b > start && *a < end)
        .map(|(a, b)| (a.max(start) - start, b.min(end) - start))
        .collect()
}

/// ffmpeg expression that is non-zero while `t` is inside any of `ranges`.
fn ranges_expr(ranges: &[(f64, f64)]) -> String {
    ranges
        .iter()
        .map(|(a, b)| format!("gte(t,{a:.3})*lt(t,{b:.3})"))
        .collect::<Vec<_>>()
        .join("+")
}

/// Decodes a single RGBA frame at `time`, scaled to `w`x`h`.
pub fn grab_frame(tools: &Tools, path: &Path, time: f64, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let out = command(&tools.ffmpeg)
        .args(["-v", "error", "-ss", &format!("{time:.3}"), "-i"])
        .arg(path)
        .args(["-frames:v", "1", "-vf", &format!("scale={w}:{h}")])
        .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
        .output()
        .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
    let expected = (w * h * 4) as usize;
    if out.stdout.len() < expected {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let mut data = out.stdout;
    data.truncate(expected);
    Ok(data)
}

// ---------------------------------------------------------------------------
// Export

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Encoder {
    /// libx264: best quality per byte.
    X264,
    /// NVIDIA hardware encoder: several times faster, somewhat larger files at equal quality.
    Nvenc,
}

/// Whether h264_nvenc actually works here (needs an NVIDIA GPU and driver, not just the build flag).
pub fn nvenc_available(tools: &Tools) -> bool {
    command(&tools.ffmpeg)
        .args(["-v", "error", "-f", "lavfi", "-i", "color=size=256x256:duration=0.1", "-c:v", "h264_nvenc", "-f", "null", "-"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[derive(Clone, Debug, PartialEq)]
pub enum Mode {
    /// Stream copy: instant, but cuts snap to keyframes and nothing can be resized.
    Fast,
    Reencode { max_height: Option<u32>, target_mb: Option<f64>, encoder: Encoder },
}

#[derive(Clone, Debug)]
pub struct ExportSettings {
    pub input: PathBuf,
    pub output: PathBuf,
    /// Time ranges of the source to keep, in order.
    pub keep: Vec<(f64, f64)>,
    pub keep_audio: bool,
    /// Source time ranges where audio is silenced but video is kept.
    pub muted_audio: Vec<(f64, f64)>,
    pub mode: Mode,
}

pub const AUDIO_KBPS: f64 = 128.0;
const NVENC_RATE_MARGIN: f64 = 0.94;

/// Video bitrate needed to hit `target_mb` for a clip of `duration` seconds.
pub fn video_kbps_for_target(target_mb: f64, duration: f64, with_audio: bool) -> f64 {
    // 4% headroom for container overhead and encoder overshoot.
    let total_kbits = target_mb * 8.0 * 1000.0 * 0.96;
    let audio = if with_audio { AUDIO_KBPS } else { 0.0 };
    total_kbits / duration.max(0.001) - audio
}

#[derive(Default, Clone)]
pub struct Progress {
    pub fraction: f32,
    pub stage: String,
    pub finished: bool,
    pub error: Option<String>,
}

pub struct ExportJob {
    pub progress: Arc<Mutex<Progress>>,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
}

impl ExportJob {
    pub fn start(tools: Tools, settings: ExportSettings, repaint: impl Fn() + Send + 'static) -> Self {
        let job = Self {
            progress: Arc::default(),
            child: Arc::default(),
            cancelled: Arc::default(),
        };
        let runner = Runner {
            tools,
            progress: job.progress.clone(),
            child: job.child.clone(),
            cancelled: job.cancelled.clone(),
            repaint: Box::new(repaint),
        };
        std::thread::spawn(move || {
            let result = runner.run(&settings);
            let mut p = runner.progress.lock().unwrap();
            p.finished = true;
            match result {
                Ok(()) => {
                    p.fraction = 1.0;
                    p.stage = "Done".into();
                }
                Err(e) => p.error = Some(e),
            }
            drop(p);
            (runner.repaint)();
        });
        job
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
    }
}

struct Runner {
    tools: Tools,
    progress: Arc<Mutex<Progress>>,
    child: Arc<Mutex<Option<Child>>>,
    cancelled: Arc<AtomicBool>,
    repaint: Box<dyn Fn() + Send>,
}

impl Runner {
    fn run(&self, s: &ExportSettings) -> Result<(), String> {
        if s.keep.is_empty() {
            return Err("nothing left to export".into());
        }
        let tmp = std::env::temp_dir().join(format!(
            "kerf_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&tmp).map_err(|e| format!("cannot create temp dir: {e}"))?;
        let result = match &s.mode {
            Mode::Fast => self.run_fast(s, &tmp),
            Mode::Reencode { max_height, target_mb, encoder } => self.run_reencode(s, *max_height, *target_mb, *encoder, &tmp),
        };
        let _ = std::fs::remove_dir_all(&tmp);
        if result.is_err() {
            let _ = std::fs::remove_file(&s.output);
        }
        result
    }

    /// Cuts each kept range losslessly, then joins them. (Feeding the source to the
    /// concat demuxer with inpoint/outpoint would skip the temp files, but with
    /// mid-GOP cuts it emits overlapping timestamps at every join.)
    fn run_fast(&self, s: &ExportSettings, tmp: &Path) -> Result<(), String> {
        // Silenced ranges force an audio re-encode; it's applied to every part so the
        // concat demuxer sees identical audio streams.
        let reencode_audio = s.keep_audio && !s.muted_audio.is_empty();
        let maps = |args: &mut Vec<OsString>, start: f64, end: f64| {
            args.extend(["-map", "0:v:0"].map(OsString::from));
            if !s.keep_audio {
                args.extend(["-c", "copy"].map(OsString::from));
            } else if !reencode_audio {
                args.extend(["-map", "0:a:0", "-c", "copy"].map(OsString::from));
            } else {
                args.extend(["-map", "0:a:0", "-c:v", "copy", "-c:a", audio_codec_for(&s.output), "-b:a"].map(OsString::from));
                args.push(format!("{AUDIO_KBPS}k").into());
                // Input seeking resets timestamps, so shift ranges to the part's start.
                let local = local_ranges(&s.muted_audio, start, end);
                if !local.is_empty() {
                    args.push("-af".into());
                    args.push(format!("volume=0:enable='{}'", ranges_expr(&local)).into());
                }
            }
            args.extend(["-avoid_negative_ts", "make_zero"].map(OsString::from));
        };

        let ext = s.output.extension().map(|e| e.to_string_lossy().into_owned()).unwrap_or("mp4".into());
        let n = s.keep.len();
        let mut parts = Vec::new();
        for (i, &(start, end)) in s.keep.iter().enumerate() {
            // A single kept range goes straight to the output file.
            let target = if n == 1 { s.output.clone() } else { tmp.join(format!("part{i}.{ext}")) };
            let mut args: Vec<OsString> = vec!["-ss".into(), format!("{start:.3}").into(), "-i".into(), s.input.clone().into()];
            args.extend(["-t".into(), format!("{:.3}", end - start).into()]);
            maps(&mut args, start, end);
            args.push(target.clone().into());
            let span = if n == 1 { 1.0 } else { 0.9 / n as f32 };
            self.step(&format!("Cutting part {}/{n}", i + 1), args, end - start, i as f32 * span, span)?;
            parts.push(target);
        }
        if n == 1 {
            return Ok(());
        }

        let list = tmp.join("parts.txt");
        let body: String = parts
            .iter()
            .map(|p| format!("file '{}'\n", p.to_string_lossy().replace('\\', "/").replace('\'', "'\\''")))
            .collect();
        std::fs::write(&list, body).map_err(|e| format!("cannot write concat list: {e}"))?;
        let mut args: Vec<OsString> = ["-f", "concat", "-safe", "0", "-i"].map(OsString::from).to_vec();
        args.push(list.into());
        args.extend(["-c", "copy"].map(OsString::from));
        args.push(s.output.clone().into());
        let total: f64 = s.keep.iter().map(|(a, b)| b - a).sum();
        self.step("Joining parts", args, total, 0.9, 0.1)
    }

    fn run_reencode(
        &self,
        s: &ExportSettings,
        max_height: Option<u32>,
        target_mb: Option<f64>,
        encoder: Encoder,
        tmp: &Path,
    ) -> Result<(), String> {
        let total: f64 = s.keep.iter().map(|(a, b)| b - a).sum();

        // Every kept range is its own seeked input, so removed parts are never decoded.
        // Input seeking is frame-accurate when re-encoding.
        let inputs = || -> Vec<OsString> {
            let mut a = Vec::new();
            for (start, end) in &s.keep {
                if *start > 0.0 {
                    a.extend(["-ss".into(), format!("{start:.3}").into()]);
                }
                a.extend(["-t".into(), format!("{:.3}", end - start).into(), "-i".into(), s.input.clone().into()]);
            }
            a
        };
        let graph = |with_audio: bool| -> String {
            let mut g = String::new();
            let mut pads = String::new();
            for (i, &(start, end)) in s.keep.iter().enumerate() {
                pads.push_str(&format!("[{i}:v:0]"));
                if with_audio {
                    // Each input starts at t=0, so mute ranges are shifted to the segment.
                    let local = local_ranges(&s.muted_audio, start, end);
                    if local.is_empty() {
                        pads.push_str(&format!("[{i}:a:0]"));
                    } else {
                        g.push_str(&format!("[{i}:a:0]volume=0:enable='{}'[a{i}];", ranges_expr(&local)));
                        pads.push_str(&format!("[a{i}]"));
                    }
                }
            }
            let n = s.keep.len();
            g.push_str(&format!("{pads}concat=n={n}:v=1:a={}[vc]", with_audio as u8));
            if with_audio {
                g.push_str("[aout]");
            }
            g.push_str(";[vc]");
            if let Some(h) = max_height {
                g.push_str(&format!("scale=-2:'min(ih,{h})',"));
            }
            g.push_str("format=yuv420p[vout]");
            g
        };
        let base = |with_audio: bool| -> Vec<OsString> {
            let mut a = inputs();
            a.extend(["-filter_complex".into(), graph(with_audio).into(), "-map".into(), "[vout]".into()]);
            if with_audio {
                a.extend(["-map", "[aout]", "-c:a", "aac", "-b:a"].map(OsString::from));
                a.push(format!("{AUDIO_KBPS}k").into());
            } else {
                a.push("-an".into());
            }
            a.extend(
                match encoder {
                    Encoder::X264 => ["-c:v", "libx264", "-preset", "medium"],
                    Encoder::Nvenc => ["-c:v", "h264_nvenc", "-preset", "p5"],
                }
                .map(OsString::from),
            );
            a
        };
        let finish = |mut a: Vec<OsString>| {
            a.extend(["-movflags", "+faststart"].map(OsString::from));
            a.push(s.output.clone().into());
            a
        };

        let Some(mb) = target_mb else {
            let mut args = base(s.keep_audio);
            let quality: &[&str] = match encoder {
                Encoder::X264 => &["-crf", "23"],
                Encoder::Nvenc => &["-tune", "hq", "-rc", "vbr", "-cq", "24", "-b:v", "0"],
            };
            args.extend(quality.iter().map(OsString::from));
            return self.step("Encoding", finish(args), total, 0.0, 1.0);
        };

        let kbps = video_kbps_for_target(mb, total, s.keep_audio);
        if kbps < 50.0 {
            return Err(format!("{mb} MB is too small for {total:.1} s of video"));
        }
        match encoder {
            Encoder::Nvenc => {
                // NVENC does its two passes internally in a single run, but its rate
                // control overshoots more than x264's, so aim a little lower.
                let kbps = (kbps * NVENC_RATE_MARGIN).floor();
                let mut args = base(s.keep_audio);
                args.extend(["-tune", "hq", "-rc", "vbr", "-multipass", "fullres"].map(OsString::from));
                args.extend(["-b:v".into(), format!("{kbps}k").into(), "-maxrate".into(), format!("{}k", kbps * 1.5).into()]);
                args.extend(["-bufsize".into(), format!("{}k", kbps * 2.0).into()]);
                self.step("Encoding (GPU)", finish(args), total, 0.0, 1.0)
            }
            Encoder::X264 => {
                let bitrate = format!("{}k", kbps.floor());
                let log = tmp.join("pass");
                let null = if cfg!(windows) { "NUL" } else { "/dev/null" };

                let mut p1 = base(false);
                p1.extend(["-b:v".into(), bitrate.clone().into(), "-pass".into(), "1".into(), "-passlogfile".into(), log.clone().into()]);
                p1.extend(["-f", "null", null].map(OsString::from));
                self.step("Encoding (pass 1/2)", p1, total, 0.0, 0.4)?;

                let mut p2 = base(s.keep_audio);
                p2.extend(["-b:v".into(), bitrate.into(), "-pass".into(), "2".into(), "-passlogfile".into(), log.into()]);
                self.step("Encoding (pass 2/2)", finish(p2), total, 0.4, 0.6)
            }
        }
    }

    /// Runs one ffmpeg invocation, mapping its progress onto [base, base + span].
    fn step(&self, stage: &str, args: Vec<OsString>, duration: f64, base: f32, span: f32) -> Result<(), String> {
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("Cancelled".into());
        }
        self.set(|p| {
            p.stage = stage.to_string();
            p.fraction = base;
        });

        let mut child = command(&self.tools.ffmpeg)
            .args(["-y", "-v", "error", "-nostats", "-progress", "pipe:1"])
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
        let stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let err_reader = std::thread::spawn(move || {
            let mut s = String::new();
            let _ = stderr.read_to_string(&mut s);
            s
        });
        *self.child.lock().unwrap() = Some(child);
        // Cancel may have fired between the check above and storing the child.
        if self.cancelled.load(Ordering::SeqCst) {
            if let Some(c) = self.child.lock().unwrap().as_mut() {
                let _ = c.kill();
            }
        }

        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            // ffmpeg reports both keys in microseconds.
            let us = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
                .and_then(|v| v.parse::<f64>().ok());
            if let Some(us) = us {
                let f = (us / 1e6 / duration.max(0.001)).clamp(0.0, 1.0) as f32;
                self.set(|p| p.fraction = base + f * span);
            }
        }

        let status = self.child.lock().unwrap().take().unwrap().wait();
        let err = err_reader.join().unwrap_or_default();
        if self.cancelled.load(Ordering::SeqCst) {
            return Err("Cancelled".into());
        }
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("ffmpeg failed ({s}):\n{}", err.trim())),
            Err(e) => Err(format!("ffmpeg failed: {e}")),
        }
    }

    fn set(&self, f: impl FnOnce(&mut Progress)) {
        f(&mut self.progress.lock().unwrap());
        (self.repaint)();
    }
}

#[cfg(test)]
mod tests {
    //! End-to-end tests; need ffmpeg/ffprobe on PATH.
    use super::*;

    fn fixture(tools: &Tools, dir: &Path) -> PathBuf {
        let input = dir.join("in.mp4");
        let ok = command(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1280x720:rate=30:duration=20"])
            .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=20"])
            .args(["-c:v", "libx264", "-g", "30", "-c:a", "aac", "-shortest"])
            .arg(&input)
            .status()
            .unwrap()
            .success();
        assert!(ok);
        input
    }

    fn export(tools: &Tools, input: &Path, out: &Path, keep: Vec<(f64, f64)>, keep_audio: bool, mode: Mode) -> MediaInfo {
        export_muted(tools, input, out, keep, keep_audio, vec![], mode)
    }

    fn export_muted(
        tools: &Tools,
        input: &Path,
        out: &Path,
        keep: Vec<(f64, f64)>,
        keep_audio: bool,
        muted_audio: Vec<(f64, f64)>,
        mode: Mode,
    ) -> MediaInfo {
        let settings = ExportSettings { input: input.into(), output: out.into(), keep, keep_audio, muted_audio, mode };
        let job = ExportJob::start(tools.clone(), settings, || {});
        while !job.progress.lock().unwrap().finished {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if let Some(e) = &job.progress.lock().unwrap().error {
            panic!("export failed: {e}");
        }
        probe(tools, out).unwrap()
    }

    /// Loudest sample in dB within [start, start + dur] of `path`.
    fn max_volume(tools: &Tools, path: &Path, start: f64, dur: f64) -> f64 {
        let out = command(&tools.ffmpeg)
            .args(["-hide_banner", "-ss", &start.to_string(), "-t", &dur.to_string(), "-i"])
            .arg(path)
            .args(["-vn", "-af", "volumedetect", "-f", "null", "-"])
            .output()
            .unwrap();
        let err = String::from_utf8_lossy(&out.stderr);
        let line = err.lines().find(|l| l.contains("max_volume:")).expect("volumedetect output");
        line.split("max_volume:").nth(1).unwrap().trim().trim_end_matches("dB").trim().parse().unwrap()
    }

    #[test]
    fn all_modes() {
        let tools = Tools::locate().expect("ffmpeg on PATH");
        let dir = std::env::temp_dir().join("kerf_test");
        std::fs::create_dir_all(&dir).unwrap();
        let input = fixture(&tools, &dir);
        let info = probe(&tools, &input).unwrap();
        assert!(info.has_audio && info.width == 1280 && (info.duration - 20.0).abs() < 0.1);

        let frame = grab_frame(&tools, &input, 5.0, 640, 360).unwrap();
        assert_eq!(frame.len(), 640 * 360 * 4);

        let keep = vec![(0.0, 4.0), (8.0, 12.0), (15.0, 20.0)]; // 13 s

        let fast = export(&tools, &input, &dir.join("fast.mp4"), keep.clone(), true, Mode::Fast);
        println!("fast: {fast:?}");
        assert!((fast.duration - 13.0).abs() < 1.5 && fast.has_audio);

        let muted = export(&tools, &input, &dir.join("muted.mp4"), keep.clone(), false, Mode::Fast);
        assert!(!muted.has_audio);

        let crf = export(&tools, &input, &dir.join("crf.mp4"), keep.clone(), true,
            Mode::Reencode { max_height: Some(480), target_mb: None, encoder: Encoder::X264 });
        println!("crf: {crf:?}");
        assert!((crf.duration - 13.0).abs() < 0.2 && crf.height == 480 && crf.has_audio);

        let sized_path = dir.join("sized.mp4");
        let sized = export(&tools, &input, &sized_path, keep.clone(), false,
            Mode::Reencode { max_height: None, target_mb: Some(2.0), encoder: Encoder::X264 });
        let bytes = std::fs::metadata(&sized_path).unwrap().len();
        println!("sized: {sized:?} {bytes} bytes");
        assert!((sized.duration - 13.0).abs() < 0.2 && !sized.has_audio && sized.height == 720);
        assert!(bytes <= 2_000_000 && bytes > 1_400_000, "size {bytes}");

        // Source 9-11 s is silenced; after cutting it sits at 5-7 s of the output.
        for (name, mode) in [("mute_fast.mp4", Mode::Fast), ("mute_crf.mp4", Mode::Reencode { max_height: None, target_mb: None, encoder: Encoder::X264 })] {
            let path = dir.join(name);
            let info = export_muted(&tools, &input, &path, keep.clone(), true, vec![(9.0, 11.0)], mode);
            let (loud, quiet) = (max_volume(&tools, &path, 1.0, 2.0), max_volume(&tools, &path, 5.3, 1.4));
            println!("{name}: {info:?} loud {loud} dB, quiet {quiet} dB");
            assert!(info.has_audio && loud > -30.0 && quiet < -80.0);
        }
        assert!((info.fps - 30.0).abs() < 0.01);
        assert!(waveform(&tools, &input).unwrap().len() >= 1990);

        if nvenc_available(&tools) {
            let path = dir.join("gpu_sized.mp4");
            let info = export_muted(&tools, &input, &path, keep.clone(), true, vec![(9.0, 11.0)],
                Mode::Reencode { max_height: Some(480), target_mb: Some(2.0), encoder: Encoder::Nvenc });
            let bytes = std::fs::metadata(&path).unwrap().len();
            let quiet = max_volume(&tools, &path, 5.3, 1.4);
            println!("gpu_sized: {info:?} {bytes} bytes, quiet {quiet} dB");
            assert!((info.duration - 13.0).abs() < 0.2 && info.height == 480 && quiet < -80.0);
            assert!(bytes <= 2_000_000 && bytes > 1_300_000, "size {bytes}");
        }

        // Portrait phone video: stored 640x360 with a 90° rotation flag.
        let rot = dir.join("rot.mp4");
        assert!(command(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-display_rotation", "90", "-i"])
            .arg(&input)
            .args(["-t", "2", "-c", "copy"])
            .arg(&rot)
            .status()
            .unwrap()
            .success());
        let ri = probe(&tools, &rot).unwrap();
        assert_eq!((ri.width, ri.height), (720, 1280));
        assert_eq!(grab_frame(&tools, &rot, 1.0, 180, 320).unwrap().len(), 180 * 320 * 4);

        // WebM: fast cut with a muted range needs Opus, not AAC.
        let webm = dir.join("in.webm");
        assert!(command(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-i"])
            .arg(&input)
            .args(["-t", "6", "-c:v", "libvpx-vp9", "-g", "30", "-deadline", "realtime", "-cpu-used", "8", "-c:a", "libopus"])
            .arg(&webm)
            .status()
            .unwrap()
            .success());
        let wi = export_muted(&tools, &webm, &dir.join("cut.webm"), vec![(0.0, 2.0), (3.0, 6.0)], true, vec![(4.0, 5.0)], Mode::Fast);
        assert!(wi.has_audio && (wi.duration - 5.0).abs() < 1.0, "{wi:?}");

        let single = export(&tools, &input, &dir.join("single.mkv"), vec![(2.0, 6.0)], true, Mode::Fast);
        assert!((single.duration - 4.0).abs() < 1.5);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

#[cfg(test)]
mod bench {
    //! `cargo test --release bench -- --ignored --nocapture`
    use super::*;
    use std::time::Instant;

    fn run(tools: &Tools, input: &Path, out: &Path, keep_audio: bool, muted: Vec<(f64, f64)>, mode: Mode) -> f64 {
        // Keep 45 of 90 seconds, in 3 pieces.
        let keep = vec![(5.0, 20.0), (35.0, 50.0), (70.0, 85.0)];
        let settings = ExportSettings { input: input.into(), output: out.into(), keep, keep_audio, muted_audio: muted, mode };
        let began = Instant::now();
        let job = ExportJob::start(tools.clone(), settings, || {});
        while !job.progress.lock().unwrap().finished {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        if let Some(e) = &job.progress.lock().unwrap().error {
            panic!("export failed: {e}");
        }
        began.elapsed().as_secs_f64()
    }

    #[test]
    #[ignore]
    fn bench() {
        let tools = Tools::locate().unwrap();
        let dir = std::env::temp_dir().join("kerf_bench");
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("src.mp4");
        if !input.exists() {
            assert!(command(&tools.ffmpeg)
                .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30:duration=90,noise=alls=12:allf=t"])
                .args(["-f", "lavfi", "-i", "sine=duration=90", "-c:v", "libx264", "-preset", "veryfast", "-crf", "20", "-g", "60"])
                .args(["-c:a", "aac", "-shortest"])
                .arg(&input)
                .status()
                .unwrap()
                .success());
        }
        let cases: Vec<(&str, bool, Vec<(f64, f64)>, Mode)> = vec![
            ("fast", true, vec![], Mode::Fast),
            ("fast+mute range", true, vec![(40.0, 45.0)], Mode::Fast),
            ("crf", true, vec![(40.0, 45.0)], Mode::Reencode { max_height: None, target_mb: None, encoder: Encoder::X264 }),
            ("crf 720p", true, vec![], Mode::Reencode { max_height: Some(720), target_mb: None, encoder: Encoder::X264 }),
            ("target 20MB", true, vec![], Mode::Reencode { max_height: None, target_mb: Some(20.0), encoder: Encoder::X264 }),
            ("gpu crf", true, vec![(40.0, 45.0)], Mode::Reencode { max_height: None, target_mb: None, encoder: Encoder::Nvenc }),
            ("gpu 720p", true, vec![], Mode::Reencode { max_height: Some(720), target_mb: None, encoder: Encoder::Nvenc }),
            ("gpu target 20MB", true, vec![], Mode::Reencode { max_height: None, target_mb: Some(20.0), encoder: Encoder::Nvenc }),
        ];
        for (name, audio, muted, mode) in cases {
            let out = dir.join("out.mp4");
            let secs = run(&tools, &input, &out, audio, muted, mode);
            let size = std::fs::metadata(&out).unwrap().len() as f64 / 1e6;
            println!("BENCH {name:<16} {secs:6.2} s  {size:6.2} MB");
        }
        // The grainy source alone is over 1 GB; don't leave it behind.
        let _ = std::fs::remove_dir_all(&dir);
    }
}
