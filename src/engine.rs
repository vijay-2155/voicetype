use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

pub const RATE: usize = 16000;

const MODEL_FILE: &str = "ggml-large-v3-turbo-q5_0.bin";

pub fn model_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("VOICETYPE_MODELS") {
        return PathBuf::from(dir);
    }
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"))
        .join("voicetype/models")
}

pub struct Engine {
    state: Option<WhisperState>,
    threads: i32,
}

impl Engine {
    pub fn new() -> Self {
        prefer_discrete_gpu();
        if std::env::var_os("VOICETYPE_DEBUG").is_none() {
            whisper_rs::install_logging_hooks();
        }
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32;
        Self { state: None, threads }
    }

    pub fn preload(&mut self) -> Result<()> {
        self.state().map(|_| ())
    }

    fn state(&mut self) -> Result<&mut WhisperState> {
        if self.state.is_none() {
            let path = model_dir().join(MODEL_FILE);
            if !path.exists() {
                bail!("model not installed: {}", path.display());
            }
            let t = Instant::now();
            let mut params = WhisperContextParameters::default();
            params.use_gpu(true).flash_attn(true);
            let ctx = WhisperContext::new_with_params(&path, params)
                .with_context(|| format!("loading {}", path.display()))?;
            let state = ctx.create_state().context("creating whisper state")?;
            eprintln!("loaded {MODEL_FILE} in {:.1}s", t.elapsed().as_secs_f32());
            self.state = Some(state);
        }
        Ok(self.state.as_mut().unwrap())
    }

    pub fn transcribe(&mut self, audio: &[f32]) -> Result<String> {
        let words = crate::history::words();
        let text = self.run(audio, words.prompt.as_deref())?;
        Ok(crate::history::apply_fixes(&text, &words.fixes))
    }

    fn run(&mut self, audio: &[f32], prompt: Option<&str>) -> Result<String> {
        if audio.len() < RATE / 4 || rms(audio) < 0.004 {
            return Ok(String::new());
        }
        let threads = self.threads;
        let state = self.state()?;
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        p.set_language(Some("en"));
        p.set_n_threads(threads);
        p.set_no_context(true);
        p.set_no_timestamps(true);
        p.set_suppress_blank(true);
        p.set_suppress_nst(true);
        p.set_print_special(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_timestamps(false);
        if let Some(prompt) = prompt {
            p.set_initial_prompt(prompt);
        }
        let mut padded;
        let data = if audio.len() < RATE + RATE / 10 {
            padded = audio.to_vec();
            padded.resize(RATE + RATE / 10, 0.0);
            &padded[..]
        } else {
            audio
        };
        state.full(p, data).context("whisper run")?;
        let mut out = Vec::new();
        for seg in state.as_iter() {
            let text = seg.to_str_lossy().map(|s| s.trim().to_string()).unwrap_or_default();
            if !text.is_empty() && !is_noise(&text) {
                out.push(text);
            }
        }
        Ok(out.join(" "))
    }
}

fn prefer_discrete_gpu() {
    if std::env::var_os("GGML_VK_VISIBLE_DEVICES").is_some() {
        return;
    }
    let Ok(out) = std::process::Command::new("vulkaninfo").arg("--summary").output() else { return };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut gpu = None;
    for line in text.lines() {
        if let Some(n) = line.strip_prefix("GPU").and_then(|r| r.strip_suffix(':')) {
            gpu = n.parse::<u32>().ok();
        } else if line.contains("PHYSICAL_DEVICE_TYPE_DISCRETE_GPU") {
            if let Some(n) = gpu {
                unsafe { std::env::set_var("GGML_VK_VISIBLE_DEVICES", n.to_string()) };
                return;
            }
        }
    }
}

fn is_noise(text: &str) -> bool {
    let t = text.trim().to_lowercase();
    t.is_empty() || matches!(t.as_str(), "[blank_audio]" | "[music]" | "[silence]" | "[inaudible]" | "(music)" | "(silence)" | "(inaudible)")
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noise_filter_preserves_spoken_words() {
        for text in ["Thank you.", "You", "So", "Bye!", "Music", "Silence"] {
            assert!(!is_noise(text), "discarded spoken text: {text}");
        }
        for text in ["", "  ", "[BLANK_AUDIO]", "[Music]", "(silence)", "[inaudible]"] {
            assert!(is_noise(text), "kept noise marker: {text}");
        }
    }
}
