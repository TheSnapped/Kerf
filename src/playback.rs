//! Real-time playback: one ffmpeg process streams RGBA frames, another streams
//! PCM into a cpal output stream. The audio device is the master clock.

use crate::ffmpeg::{command, Tools};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};
use std::cell::Cell;
use std::collections::VecDeque;
use std::io::{BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::mpsc::{sync_channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub struct PlaybackParams {
    pub path: PathBuf,
    pub start: f64,
    pub fps: f64,
    pub width: u32,
    pub height: u32,
    pub has_audio: bool,
    /// Source ranges that play silent (removed or audio-muted parts).
    pub silent: Vec<(f64, f64)>,
    pub mute: Arc<AtomicBool>,
}

struct AudioOut {
    _stream: cpal::Stream,
    rate: f64,
    /// Sample frames handed to the device so far.
    played: Arc<AtomicU64>,
    buf: Arc<Mutex<VecDeque<f32>>>,
    eof: Arc<AtomicBool>,
}

pub struct Playback {
    start: f64,
    fps: f64,
    frames: Receiver<Vec<u8>>,
    next_index: u64,
    pending: Option<Vec<u8>>,
    video_done: bool,
    audio: Option<AudioOut>,
    /// Wall-clock fallback: used without audio, or once the audio stream has ended.
    wall: Cell<Option<(Instant, f64)>>,
    children: Vec<Child>,
    stop: Arc<AtomicBool>,
}

impl Playback {
    pub fn start(tools: &Tools, p: PlaybackParams, repaint: impl Fn() + Send + 'static) -> Result<Self, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let mut children = Vec::new();
        // Cap the decode rate; nobody needs 240 fps in a preview.
        let fps = p.fps.min(60.0);

        let mut video = command(&tools.ffmpeg)
            .args(["-v", "error", "-ss", &format!("{:.3}", p.start), "-i"])
            .arg(&p.path)
            .args(["-an", "-vf", &format!("fps={fps},scale={}:{}", p.width, p.height)])
            .args(["-f", "rawvideo", "-pix_fmt", "rgba", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
        let mut stdout = BufReader::with_capacity(1 << 20, video.stdout.take().unwrap());
        children.push(video);
        let (tx, frames) = sync_channel(8);
        let frame_len = (p.width * p.height * 4) as usize;
        std::thread::spawn(move || loop {
            let mut frame = vec![0u8; frame_len];
            if stdout.read_exact(&mut frame).is_err() || tx.send(frame).is_err() {
                break;
            }
            repaint();
        });

        let audio = if p.has_audio {
            match start_audio(tools, &p, &stop) {
                Ok((out, child)) => {
                    children.push(child);
                    Some(out)
                }
                // No output device etc.: fall back to silent playback on the wall clock.
                Err(_) => None,
            }
        } else {
            None
        };

        Ok(Self {
            start: p.start,
            fps,
            frames,
            next_index: 0,
            pending: None,
            video_done: false,
            audio,
            wall: Cell::new(None),
            children,
            stop,
        })
    }

    /// Current playback position in source seconds.
    pub fn time(&self) -> f64 {
        if let Some((at, base)) = self.wall.get() {
            return base + at.elapsed().as_secs_f64();
        }
        match &self.audio {
            Some(a) => {
                let t = self.start + a.played.load(Relaxed) as f64 / a.rate;
                if a.eof.load(Relaxed) && a.buf.lock().unwrap().is_empty() {
                    self.wall.set(Some((Instant::now(), t)));
                }
                t
            }
            // The wall clock starts with the first decoded frame.
            None => self.start,
        }
    }

    /// The newest frame that is due, dropping any we're too late for.
    pub fn next_frame(&mut self) -> Option<Vec<u8>> {
        let mut out = None;
        loop {
            if self.pending.is_none() {
                match self.frames.try_recv() {
                    Ok(f) => {
                        if self.audio.is_none() && self.wall.get().is_none() {
                            self.wall.set(Some((Instant::now(), self.start)));
                        }
                        self.pending = Some(f);
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.video_done = true;
                        break;
                    }
                }
            }
            let pts = self.start + self.next_index as f64 / self.fps;
            if pts > self.time() {
                break;
            }
            out = self.pending.take();
            self.next_index += 1;
        }
        out
    }

    pub fn finished(&self) -> bool {
        let audio_done = self.audio.as_ref().is_none_or(|a| a.eof.load(Relaxed) && a.buf.lock().unwrap().is_empty());
        self.video_done && self.pending.is_none() && audio_done
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
        for c in &mut self.children {
            let _ = c.kill();
            let _ = c.wait();
        }
    }
}

fn start_audio(tools: &Tools, p: &PlaybackParams, stop: &Arc<AtomicBool>) -> Result<(AudioOut, Child), String> {
    let device = cpal::default_host().default_output_device().ok_or("no audio output device")?;
    let supported = device.default_output_config().map_err(|e| e.to_string())?;
    let format = supported.sample_format();
    let config: cpal::StreamConfig = supported.into();
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;

    let mut child = command(&tools.ffmpeg)
        .args(["-v", "error", "-ss", &format!("{:.3}", p.start), "-i"])
        .arg(&p.path)
        .args(["-vn", "-ac", &channels.to_string(), "-ar", &rate.to_string(), "-f", "f32le", "-"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("failed to run ffmpeg: {e}"))?;
    let mut stdout = child.stdout.take().unwrap();

    let buf = Arc::new(Mutex::new(VecDeque::<f32>::new()));
    let eof = Arc::new(AtomicBool::new(false));
    let played = Arc::new(AtomicU64::new(0));

    // Feeder: keeps about half a second buffered ahead of the device.
    {
        let (buf, eof, stop) = (buf.clone(), eof.clone(), stop.clone());
        let cap = rate as usize * channels / 2;
        std::thread::spawn(move || {
            let mut chunk = vec![0u8; 4 * channels * 1024];
            while !stop.load(Relaxed) && stdout.read_exact(&mut chunk).is_ok() {
                while !stop.load(Relaxed) && buf.lock().unwrap().len() > cap {
                    std::thread::sleep(Duration::from_millis(5));
                }
                let samples = chunk.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]));
                buf.lock().unwrap().extend(samples);
            }
            eof.store(true, Relaxed);
        });
    }

    let shared = Shared {
        buf: buf.clone(),
        played: played.clone(),
        start: p.start,
        rate: rate as f64,
        channels,
        silent: p.silent.clone(),
        mute: p.mute.clone(),
    };
    let stream = match format {
        cpal::SampleFormat::F32 => build_stream::<f32>(&device, &config, shared),
        cpal::SampleFormat::I16 => build_stream::<i16>(&device, &config, shared),
        cpal::SampleFormat::U16 => build_stream::<u16>(&device, &config, shared),
        other => Err(format!("unsupported sample format {other}")),
    }
    .inspect_err(|_| {
        let _ = child.kill();
    })?;
    stream.play().map_err(|e| e.to_string())?;

    Ok((AudioOut { _stream: stream, rate: rate as f64, played, buf, eof }, child))
}

struct Shared {
    buf: Arc<Mutex<VecDeque<f32>>>,
    played: Arc<AtomicU64>,
    start: f64,
    rate: f64,
    channels: usize,
    silent: Vec<(f64, f64)>,
    mute: Arc<AtomicBool>,
}

fn build_stream<T: SizedSample + FromSample<f32>>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    s: Shared,
) -> Result<cpal::Stream, String> {
    device
        .build_output_stream(
            config,
            move |data: &mut [T], _| {
                let mut buf = s.buf.lock().unwrap();
                let muted = s.mute.load(Relaxed);
                for frame in data.chunks_mut(s.channels) {
                    // On underrun output silence without advancing the clock,
                    // so video waits for audio instead of drifting ahead.
                    if buf.len() < s.channels {
                        frame.fill(T::EQUILIBRIUM);
                        continue;
                    }
                    let n = s.played.fetch_add(1, Relaxed);
                    let t = s.start + n as f64 / s.rate;
                    let silent = muted || s.silent.iter().any(|&(a, b)| t >= a && t < b);
                    for out in frame.iter_mut() {
                        let v = buf.pop_front().unwrap_or(0.0);
                        *out = T::from_sample(if silent { 0.0 } else { v });
                    }
                }
            },
            |e| eprintln!("audio stream error: {e}"),
            None,
        )
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plays 2 s from the middle of a clip and checks the clock and frame pacing.
    #[test]
    fn plays_in_real_time() {
        let tools = Tools::locate().expect("ffmpeg on PATH");
        let input = std::env::temp_dir().join("kerf_play_test.mp4");
        assert!(command(&tools.ffmpeg)
            .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=10"])
            .args(["-f", "lavfi", "-i", "sine=duration=10", "-c:v", "libx264", "-c:a", "aac", "-shortest"])
            .arg(&input)
            .status()
            .unwrap()
            .success());

        let params = PlaybackParams {
            path: input,
            start: 3.0,
            fps: 30.0,
            width: 320,
            height: 180,
            has_audio: true,
            silent: vec![],
            mute: Arc::new(AtomicBool::new(true)), // don't beep during tests
        };
        let mut pb = Playback::start(&tools, params, || {}).unwrap();
        println!("audio clock: {}", pb.audio.is_some());
        let began = Instant::now();
        let mut shown = 0;
        while began.elapsed() < Duration::from_secs(2) {
            if pb.next_frame().is_some() {
                shown += 1;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let t = pb.time();
        println!("t = {t:.3}, frames shown = {shown}");
        // Allow for decoder / device start-up latency.
        assert!(t > 4.6 && t < 5.05, "clock {t}");
        drop(pb);
        let _ = std::fs::remove_file(std::env::temp_dir().join("kerf_play_test.mp4"));
        assert!(shown > 45 && shown <= 62, "frames {shown}");
    }
}
