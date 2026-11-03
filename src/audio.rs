use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{Context, Result};
use nnnoiseless::DenoiseState;

use crate::engine::{RATE, rms};

const MIC_RATE: usize = 48000;
const FRAME: usize = DenoiseState::FRAME_SIZE;
const STEP: usize = MIC_RATE / RATE;
const TAPS: usize = 63;
const SPEECH: f32 = 0.6;

pub struct Cleaner {
    denoise: Box<DenoiseState<'static>>,
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
        Self { denoise: DenoiseState::new(), taps, pending: Vec::new(), tail: vec![0.0; TAPS - 1], audio: Vec::new(), vad: Vec::new() }
    }

    pub fn feed(&mut self, input: &[f32]) -> usize {
        self.pending.extend(input.iter().map(|s| s * 32768.0));
        let before = self.audio.len();
        let mut out = [0f32; FRAME];
        while self.pending.len() >= FRAME {
            let frame: Vec<f32> = self.pending.drain(..FRAME).collect();
            let p = self.denoise.process_frame(&mut out, &frame);
            self.vad.push(p);
            let mut buf = std::mem::take(&mut self.tail);
            buf.extend(out.iter().map(|s| s / 32768.0));
            for k in 0..FRAME / STEP {
                let w = &buf[k * STEP..k * STEP + TAPS];
                self.audio.push(w.iter().zip(&self.taps).map(|(a, b)| a * b).sum());
            }
            self.tail = buf[buf.len() - (TAPS - 1)..].to_vec();
        }
        before
    }
}

pub fn heard_speech(vad: &[f32]) -> bool {
    vad.iter().filter(|&&p| p > SPEECH).count() >= 12
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
}

impl Recorder {
    pub fn start(mic: Option<&str>, on_level: impl Fn(f32) + Send + 'static) -> Result<Self> {
        let mut cmd = Command::new("pw-record");
        if let Some(m) = mic {
            cmd.args(["--target", m]);
        }
        let mut child = cmd
            .args(["--raw", "--rate", &MIC_RATE.to_string(), "--channels", "1", "--format", "s16", "--latency", "20ms", "-"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .context("starting pw-record")?;
        let mut out = child.stdout.take().context("no pw-record stdout")?;
        let samples = Arc::new(Mutex::new(Vec::with_capacity(RATE * 60)));
        let vad = Arc::new(Mutex::new(Vec::new()));
        let (sink, vad_sink) = (samples.clone(), vad.clone());
        let reader = std::thread::spawn(move || {
            let mut cleaner = Cleaner::new();
            let mut buf = [0u8; 4800];
            let mut carry: Option<u8> = None;
            loop {
                let n = match out.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => n,
                };
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
                let vad_before = cleaner.vad.len();
                let from = cleaner.feed(&chunk);
                let fresh = &cleaner.audio[from..];
                if !fresh.is_empty() {
                    let speaking = cleaner.vad[vad_before..].iter().any(|&p| p > SPEECH);
                    on_level(if speaking { rms(fresh).max(0.02) } else { rms(fresh) * 0.3 });
                    sink.lock().unwrap().extend_from_slice(fresh);
                    vad_sink.lock().unwrap().extend_from_slice(&cleaner.vad[vad_before..]);
                }
            }
        });
        Ok(Self { child, reader: Some(reader), samples, vad })
    }

    pub fn stop(mut self) -> (Vec<f32>, Vec<f32>) {
        std::thread::sleep(Duration::from_millis(150));
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(r) = self.reader.take() {
            let _ = r.join();
        }
        let audio = std::mem::take(&mut *self.samples.lock().unwrap());
        let vad = std::mem::take(&mut *self.vad.lock().unwrap());
        (finish_clip(&audio, &vad), vad)
    }

    pub fn cancel(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
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
