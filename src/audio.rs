use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use nnnoiseless::DenoiseState;

use crate::engine::{RATE, rms};

const MIC_RATE: usize = 48000;
const FRAME: usize = DenoiseState::FRAME_SIZE;
const STEP: usize = MIC_RATE / RATE;
const TAPS: usize = 63;
const SPEECH: f32 = 0.6;

#[derive(Clone, Copy, Default)]
pub struct InputLevel {
    pub rms: f32,
    pub peak: f32,
    pub clipped: f32,
}

impl InputLevel {
    pub fn measure(samples: &[f32]) -> Self {
        Self {
            rms: rms(samples),
            peak: samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs())),
            clipped: samples.iter().filter(|s| s.abs() >= 0.999).count() as f32 / samples.len().max(1) as f32,
        }
    }

    pub fn description(self) -> String {
        let db = 20.0 * self.rms.max(0.000001).log10();
        // shortcut: level thresholds are guidance, calibrate per device if presets are added.
        let hint = if self.clipped >= 0.01 {
            "Clipping — lower microphone volume"
        } else if self.peak < 0.0001 {
            "No input — check mute or microphone"
        } else if db < -45.0 {
            "Low input — move closer if speaking"
        } else {
            "Microphone input"
        };
        format!("{hint} · {db:.0} dBFS")
    }
}

pub struct CaptureHealth {
    last_data: Instant,
    ended: bool,
}

impl CaptureHealth {
    pub fn failed(&self) -> bool {
        self.ended || self.last_data.elapsed() > Duration::from_secs(2)
    }
}

#[derive(Default)]
struct DcBlocker {
    input: f32,
    output: f32,
}

impl DcBlocker {
    fn process(&mut self, sample: f32) -> f32 {
        // A 20 Hz high-pass at 48 kHz removes DC without cutting speech fundamentals.
        let output = sample - self.input + 0.9973854 * self.output;
        self.input = sample;
        self.output = output;
        output
    }
}

pub struct Cleaner {
    denoise: Box<DenoiseState<'static>>,
    dc: DcBlocker,
    taps: Vec<f32>,
    pending: Vec<f32>,
    tail: Vec<f32>,
    pub audio: Vec<f32>,
    pub vad: Vec<f32>,
}

impl Cleaner {
    pub fn new() -> Self {
        let fc = 7200.0 / MIC_RATE as f32;
        let mid = (TAPS / 2) as f32;
        let mut taps: Vec<f32> = (0..TAPS)
            .map(|i| {
                let x = i as f32 - mid;
                let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f32::consts::PI * fc * x).sin() / (std::f32::consts::PI * x) };
                let hann = 0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / (TAPS - 1) as f32).cos();
                sinc * hann
            })
            .collect();
        let sum: f32 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Self { denoise: DenoiseState::new(), dc: DcBlocker::default(), taps, pending: Vec::new(), tail: vec![0.0; TAPS - 1], audio: Vec::new(), vad: Vec::new() }
    }

    pub fn feed(&mut self, input: &[f32]) -> usize {
        self.pending.extend(input.iter().map(|&s| self.dc.process(s) * 32768.0));
        let before = self.audio.len();
        let mut out = [0f32; FRAME];
        let processed = self.pending.len() / FRAME * FRAME;
        for frame in self.pending[..processed].chunks_exact(FRAME) {
            let p = self.denoise.process_frame(&mut out, frame);
            self.vad.push(p);
            let mut buf = std::mem::take(&mut self.tail);
            buf.extend(out.iter().map(|s| s / 32768.0));
            for k in 0..FRAME / STEP {
                let w = &buf[k * STEP..k * STEP + TAPS];
                self.audio.push(w.iter().zip(&self.taps).map(|(a, b)| a * b).sum());
            }
            self.tail = buf[buf.len() - (TAPS - 1)..].to_vec();
        }
        self.pending.drain(..processed);
        before
    }
}

pub fn heard_speech(vad: &[f32]) -> bool {
    vad.iter().filter(|&&p| p > SPEECH).take(12).count() == 12
}

pub fn speech_after(vad: &[f32], from: usize) -> bool {
    vad.get(from..).is_some_and(|v| v.iter().any(|&p| p > SPEECH))
}

pub fn trailing_silence(vad: &[f32]) -> f32 {
    let quiet = vad.iter().rev().take_while(|&&p| p <= SPEECH).count();
    quiet as f32 * FRAME as f32 / MIC_RATE as f32
}

pub fn finish_clip(audio: &[f32], vad: &[f32]) -> Vec<f32> {
    if std::env::var_os("VOICETYPE_DEBUG").is_some() {
        eprintln!("speech frames: {} of {}", vad.iter().filter(|&&p| p > SPEECH).count(), vad.len());
    }
    if !heard_speech(vad) {
        return Vec::new();
    }
    let per = FRAME / STEP;
    let first = vad.iter().position(|&p| p > SPEECH).unwrap_or(0);
    let last = vad.iter().rposition(|&p| p > SPEECH).unwrap_or(vad.len());
    let from = (first.saturating_sub(30) * per).min(audio.len());
    let to = ((last + 45) * per).min(audio.len());
    let mut clip = audio[from..to].to_vec();
    let peak = clip.iter().fold(0f32, |m, s| m.max(s.abs()));
    if peak > 0.0 {
        let gain = (0.9 / peak).min(12.0);
        clip.iter_mut().for_each(|s| *s *= gain);
    }
    clip
}

pub struct Recorder {
    child: Child,
    reader: Option<JoinHandle<()>>,
    pub samples: Arc<Mutex<Vec<f32>>>,
    pub vad: Arc<Mutex<Vec<f32>>>,
    pub health: Arc<Mutex<CaptureHealth>>,
}

impl Recorder {
    pub fn start(mic: Option<&str>, on_level: impl Fn(InputLevel) + Send + 'static) -> Result<Self> {
        let mut cmd = Command::new("pw-record");
        if let Some(m) = mic {
            cmd.args(["--target", m]);
        }
        let mut child = cmd
            .args(["--raw", "--rate", &MIC_RATE.to_string(), "--channels", "1", "--format", "s16", "--latency", "20ms", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("starting pw-record")?;
        let mut out = child.stdout.take().context("no pw-record stdout")?;
        let samples = Arc::new(Mutex::new(Vec::with_capacity(RATE * 60)));
        let vad = Arc::new(Mutex::new(Vec::new()));
        let health = Arc::new(Mutex::new(CaptureHealth { last_data: Instant::now(), ended: false }));
        let reader_health = health.clone();
        let (sink, vad_sink) = (samples.clone(), vad.clone());
        let reader = std::thread::spawn(move || {
            let mut cleaner = Cleaner::new();
            let mut buf = [0u8; 4800];
            let mut carry: Option<u8> = None;
            let mut levels = Vec::with_capacity(MIC_RATE / 10);
            loop {
                let n = match out.read(&mut buf) {
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
                reader_health.lock().unwrap().last_data = Instant::now();
                let mut bytes = Vec::with_capacity(n + 1);
                bytes.extend(carry.take());
                bytes.extend_from_slice(&buf[..n]);
                if bytes.len() % 2 == 1 {
                    carry = bytes.pop();
                }
                let chunk: Vec<f32> = bytes
                    .chunks_exact(2)
                    .map(|b| i16::from_le_bytes([b[0], b[1]]) as f32 / 32768.0)
                    .collect();
                levels.extend_from_slice(&chunk);
                if levels.len() >= MIC_RATE / 10 {
                    on_level(InputLevel::measure(&levels));
                    levels.clear();
                }
                let vad_before = cleaner.vad.len();
                let from = cleaner.feed(&chunk);
                let fresh = &cleaner.audio[from..];
                if !fresh.is_empty() {
                    let mut samples = sink.lock().unwrap();
                    let mut vad = vad_sink.lock().unwrap();
                    samples.extend_from_slice(fresh);
                    vad.extend_from_slice(&cleaner.vad[vad_before..]);
                }
            }
            reader_health.lock().unwrap().ended = true;
        });
        Ok(Self { child, reader: Some(reader), samples, vad, health })
    }

    pub fn stop(mut self) -> (Vec<f32>, Vec<f32>, bool) {
        std::thread::sleep(Duration::from_millis(150));
        let failed = self.health.lock().unwrap().failed();
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let audio = std::mem::take(&mut *self.samples.lock().unwrap());
        let vad = std::mem::take(&mut *self.vad.lock().unwrap());
        (finish_clip(&audio, &vad), vad, failed)
    }

    pub fn cancel(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub fn write_wav(path: &std::path::Path, audio: &[f32]) -> std::io::Result<()> {
    let data: Vec<u8> = audio
        .iter()
        .flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes())
        .collect();
    let mut h = Vec::with_capacity(44 + data.len());
    h.extend_from_slice(b"RIFF");
    h.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    h.extend_from_slice(b"WAVEfmt ");
    h.extend_from_slice(&16u32.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&1u16.to_le_bytes());
    h.extend_from_slice(&(RATE as u32).to_le_bytes());
    h.extend_from_slice(&(RATE as u32 * 2).to_le_bytes());
    h.extend_from_slice(&2u16.to_le_bytes());
    h.extend_from_slice(&16u16.to_le_bytes());
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(data.len() as u32).to_le_bytes());
    h.extend_from_slice(&data);
    std::fs::write(path, h)
}

pub fn decode_file(path: &str, clean: bool) -> Result<Vec<f32>> {
    let rate = if clean { MIC_RATE } else { RATE };
    let out = Command::new("ffmpeg")
        .args(["-nostdin", "-v", "error", "-i", path, "-f", "f32le", "-ac", "1", "-ar", &rate.to_string(), "-"])
        .output()
        .context("running ffmpeg")?;
    anyhow::ensure!(out.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&out.stderr));
    let raw: Vec<f32> = out.stdout.chunks_exact(4).map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]])).collect();
    if !clean {
        return Ok(raw);
    }
    let mut c = Cleaner::new();
    c.feed(&raw);
    Ok(finish_clip(&c.audio, &c.vad))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_levels_distinguish_silence_quiet_and_clipping() {
        assert!(InputLevel::measure(&[]).description().contains("No input"));
        assert!(InputLevel::measure(&[0.0; 4800]).description().contains("No input"));
        assert!(InputLevel::measure(&[0.001; 4800]).description().contains("Low input"));
        let normal = InputLevel::measure(&[0.1, -0.1]);
        assert!((normal.rms - 0.1).abs() < 0.00001);
        assert_eq!(normal.clipped, 0.0);
        assert!(normal.description().contains("-20 dBFS"));
        let mut samples = vec![0.1; 100];
        samples[0] = -1.0;
        assert!(InputLevel::measure(&samples).description().contains("Clipping"));
        assert_eq!(InputLevel::measure(&samples).peak, 1.0);
    }

    #[test]
    fn dc_filter_removes_offset_and_preserves_speech_band() {
        let mut filter = DcBlocker::default();
        for _ in 0..MIC_RATE {
            filter.process(0.25);
        }
        assert!(filter.output.abs() < 0.00001);
        let mut input = Vec::new();
        let mut output = Vec::new();
        for i in 0..MIC_RATE {
            let sample = 0.1 * (2.0 * std::f32::consts::PI * 200.0 * i as f32 / MIC_RATE as f32).sin();
            let cleaned = filter.process(sample + 0.25);
            if i > MIC_RATE / 2 {
                input.push(sample);
                output.push(cleaned);
            }
        }
        assert!((rms(&output) / rms(&input) - 1.0).abs() < 0.02);
        assert!(output.iter().sum::<f32>().abs() / (output.len() as f32) < 0.0001);
    }

    #[test]
    fn capture_health_detects_ended_and_stalled_streams() {
        let mut health = CaptureHealth { last_data: Instant::now(), ended: false };
        assert!(!health.failed());
        health.ended = true;
        assert!(health.failed());
        health.ended = false;
        health.last_data = Instant::now() - Duration::from_secs(3);
        assert!(health.failed());
    }

    #[test]
    fn cleaning_is_independent_of_input_chunk_size() {
        let input: Vec<f32> = (0..FRAME * 24 + 17).map(|i| (i as f32 * 0.07).sin() * 0.1).collect();
        let mut whole = Cleaner::new();
        whole.feed(&input);
        let mut chunked = Cleaner::new();
        for chunk in input.chunks(317) {
            let before = chunked.audio.len();
            assert_eq!(chunked.feed(chunk), before);
        }
        assert_eq!(whole.audio.len(), 24 * FRAME / STEP);
        assert_eq!(whole.pending.len(), 17);
        assert_eq!(whole.audio, chunked.audio);
        assert_eq!(whole.vad, chunked.vad);
        assert_eq!(whole.pending, chunked.pending);
    }

    #[test]
    fn speech_gate_and_trim_keep_the_trailing_margin() {
        assert!(!heard_speech(&[0.9; 11]));
        assert!(heard_speech(&[0.9; 12]));
        let mut vad = vec![0.0; 100];
        vad[35..47].fill(0.9);
        let audio = vec![0.1; vad.len() * FRAME / STEP];
        let clip = finish_clip(&audio, &vad);
        assert_eq!(clip.len(), (46 + 45 - (35 - 30)) * FRAME / STEP);
        assert!(!speech_after(&vad, 47));
        assert!(speech_after(&vad, 46));
        assert!(finish_clip(&audio, &[0.0; 100]).is_empty());
    }
}
