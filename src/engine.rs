use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperState};

pub const RATE: usize = 16000;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Profile {
    Auto,
    En,
    Te,
}

impl Profile {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "en" => Some(Self::En),
            "te" => Some(Self::Te),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "AUTO",
            Self::En => "EN",
            Self::Te => "తెలుగు",
        }
    }

    fn model_file(self) -> &'static str {
        match self {
            Self::Te if model_dir().join("ggml-telugu-medium-q5_0.bin").exists() => "ggml-telugu-medium-q5_0.bin",
            Self::Te => "ggml-telugu-medium.bin",
            _ => "ggml-large-v3-turbo-q5_0.bin",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::En => "en",
            Self::Te => "te",
        }
    }

    fn chunk_secs(self) -> Option<usize> {
        match self {
            Self::Te => Some(8),
            _ => None,
        }
    }
}

const INDIC: [&str; 12] = ["hi", "te", "ta", "kn", "ml", "mr", "bn", "gu", "pa", "ur", "sa", "ne"];

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
    states: HashMap<&'static str, WhisperState>,
    threads: i32,
    pub auto_languages: Vec<String>,
}

impl Engine {
    pub fn new() -> Self {
        prefer_discrete_gpu();
        if std::env::var_os("VOICETYPE_DEBUG").is_none() {
            whisper_rs::install_logging_hooks();
        }
        let threads = std::thread::available_parallelism().map_or(4, |n| n.get().min(8)) as i32;
        Self { states: HashMap::new(), threads, auto_languages: vec!["en".into(), "te".into()] }
    }

    pub fn preload(&mut self, profile: Profile) -> Result<()> {
        self.state(profile).map(|_| ())
    }

    fn state(&mut self, profile: Profile) -> Result<&mut WhisperState> {
        let file = profile.model_file();
        if !self.states.contains_key(file) {
            let path = model_dir().join(file);
            if !path.exists() {
                bail!("model not installed: {}", path.display());
            }
            let t = Instant::now();
            let mut params = WhisperContextParameters::default();
            params.use_gpu(true).flash_attn(true);
            let ctx = WhisperContext::new_with_params(&path, params)
                .with_context(|| format!("loading {}", path.display()))?;
            let state = ctx.create_state().context("creating whisper state")?;
            eprintln!("loaded {file} in {:.1}s", t.elapsed().as_secs_f32());
            self.states.insert(file, state);
        }
        Ok(self.states.get_mut(file).unwrap())
    }

    fn pick_language(&mut self, audio: &[f32]) -> Result<String> {
        if self.auto_languages.len() == 1 {
            return Ok(self.auto_languages[0].clone());
        }
        let threads = self.threads as usize;
        let mut clip = audio[..audio.len().min(RATE * 30)].to_vec();
        clip.resize(clip.len().max(RATE + RATE / 10), 0.0);
        let state = self.state(Profile::Auto)?;
        state.pcm_to_mel(&clip, threads).context("mel")?;
        let (_, probs) = state.lang_detect(0, threads).context("language detection")?;
        let score = |l: &str| whisper_rs::get_lang_id(l).and_then(|i| probs.get(i as usize).copied()).unwrap_or(0.0);
        let top = (0..probs.len()).max_by(|a, b| probs[*a].total_cmp(&probs[*b])).and_then(|i| whisper_rs::get_lang_str(i as i32)).unwrap_or("en");
        let allowed = |l: &str| self.auto_languages.iter().any(|a| a == l);
        let indic: f32 = INDIC.iter().map(|l| score(l)).sum();
        let te = allowed("te") && model_dir().join(Profile::Te.model_file()).exists();
        let best = if te && top != "te" && (indic >= 0.05 || (top == "en" && score("en") < 0.85)) {
            "te".to_string()
        } else if allowed(top) && top != "te" {
            top.to_string()
        } else if te && INDIC.contains(&top) {
            "te".to_string()
        } else {
            self.auto_languages.iter().filter(|l| te || *l != "te").max_by(|a, b| score(a).total_cmp(&score(b))).cloned().unwrap_or_else(|| "en".into())
        };
        if std::env::var_os("VOICETYPE_DEBUG").is_some() {
            let all: Vec<String> = self.auto_languages.iter().map(|l| format!("{l}={:.2}", score(l))).collect();
            let mut ids: Vec<usize> = (0..probs.len()).collect();
            ids.sort_by(|a, b| probs[*b].total_cmp(&probs[*a]));
            let top5: Vec<String> = ids.iter().take(6).map(|&i| format!("{}={:.2}", whisper_rs::get_lang_str(i as i32).unwrap_or("?"), probs[i])).collect();
            eprintln!("language: {best} (top: {}; {})", top5.join(" "), all.join(" "));
        }
        Ok(best)
    }

    pub fn transcribe(&mut self, profile: Profile, audio: &[f32], fast: bool, abort: Option<&Arc<AtomicBool>>) -> Result<String> {
        if profile != Profile::Auto {
            return self.run(profile, profile.name(), audio, fast, abort);
        }
        let lang = self.pick_language(audio)?;
        let model = if lang == "te" { Profile::Te } else { Profile::Auto };
        self.run(model, &lang, audio, fast, abort)
    }

    fn run(&mut self, profile: Profile, lang: &str, audio: &[f32], fast: bool, abort: Option<&Arc<AtomicBool>>) -> Result<String> {
        let threads = self.threads;
        let pieces = match profile.chunk_secs() {
            Some(secs) => split_on_silence(audio, secs * RATE),
            None => vec![audio],
        };
        let state = self.state(profile)?;
        let mut out = Vec::new();
        for piece in pieces {
            if piece.len() < RATE / 4 || rms(piece) < 0.004 {
                continue;
            }
            let strategy = if fast || profile != Profile::Te {
                SamplingStrategy::Greedy { best_of: 1 }
            } else {
                SamplingStrategy::BeamSearch { beam_size: 5, patience: -1.0 }
            };
            let mut p = FullParams::new(strategy);
            p.set_language(Some(lang));
            p.set_n_threads(threads);
            p.set_no_context(true);
            p.set_no_timestamps(true);
            p.set_suppress_blank(true);
            p.set_suppress_nst(true);
            p.set_print_special(false);
            p.set_print_progress(false);
            p.set_print_realtime(false);
            p.set_print_timestamps(false);
            if profile == Profile::Te {
                p.set_temperature_inc(0.0);
            }
            if let Some(flag) = abort {
                let flag = flag.clone();
                p.set_abort_callback_safe(move || flag.load(Ordering::SeqCst));
            }
            let mut padded;
            let data = if piece.len() < RATE + RATE / 10 {
                padded = piece.to_vec();
                padded.resize(RATE + RATE / 10, 0.0);
                &padded[..]
            } else {
                piece
            };
            if abort.is_some_and(|f| f.load(Ordering::SeqCst)) {
                bail!("aborted");
            }
            state.full(p, data).context("whisper run")?;
            for seg in state.as_iter() {
                let text = seg.to_str_lossy().map(|s| s.trim().to_string()).unwrap_or_default();
                if !text.is_empty() && !is_noise(&text) {
                    out.push(text);
                }
            }
        }
        let text = out.join(" ");
        let bare = text.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
        if matches!(bare.as_str(), "you" | "so" | "bye" | "thank you" | "thanks for watching" | "thank you for watching") {
            return Ok(String::new());
        }
        Ok(text)
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
    let t = text.trim_matches(|c: char| !c.is_alphanumeric()).to_lowercase();
    t.is_empty() || matches!(t.as_str(), "blank_audio" | "music" | "silence" | "inaudible")
}

pub fn rms(x: &[f32]) -> f32 {
    if x.is_empty() {
        return 0.0;
    }
    (x.iter().map(|v| v * v).sum::<f32>() / x.len() as f32).sqrt()
}

fn split_on_silence(audio: &[f32], max: usize) -> Vec<&[f32]> {
    const FRAME: usize = RATE * 30 / 1000;
    let mut out = Vec::new();
    let mut start = 0;
    while audio.len() - start > max {
        let lo = start + max / 2;
        let hi = start + max;
        let mut best = hi;
        let mut best_e = f32::MAX;
        let mut i = lo;
        while i + FRAME <= hi {
            let e = rms(&audio[i..i + FRAME]);
            if e < best_e {
                best_e = e;
                best = i + FRAME / 2;
            }
            i += FRAME;
        }
        out.push(&audio[start..best]);
        start = best;
    }
    out.push(&audio[start..]);
    out
}
