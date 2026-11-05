use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use eframe::egui;
use serde_json::{Value, json};

pub struct Entry {
    pub t: u64,
    pub secs: f32,
    pub text: String,
    pub raw: Option<String>,
}

fn home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

fn path() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".local/share")).join("voicetype/history.jsonl")
}

fn settings_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home().join(".config")).join("voicetype/settings.json")
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

pub fn append(secs: f32, text: &str, raw: &str) -> std::io::Result<()> {
    let p = path();
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut line = json!({"t": now(), "s": (secs * 10.0).round() / 10.0, "text": text});
    if raw != text {
        line["raw"] = json!(raw);
    }
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p)?;
    writeln!(f, "{line}")
}

pub fn load() -> Vec<Entry> {
    let Ok(raw) = std::fs::read_to_string(path()) else { return Vec::new() };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| Entry {
            t: v["t"].as_u64().unwrap_or(0),
            secs: v["s"].as_f64().unwrap_or(0.0) as f32,
            text: v["text"].as_str().unwrap_or("").to_string(),
            raw: v["raw"].as_str().map(str::to_string),
        })
        .filter(|e| !e.text.is_empty())
        .collect()
}

pub struct Settings {
    pub mic: Option<String>,
    pub auto_stop: f32,
    pub cleanup: crate::cleanup::Mode,
}

impl Settings {
    pub fn load() -> Self {
        let v: Value = std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        Self {
            mic: v["mic"].as_str().map(str::to_string),
            auto_stop: v["auto_stop"].as_f64().unwrap_or(2.0) as f32,
            cleanup: crate::cleanup::Mode::from_setting(&v["cleanup"]),
        }
    }

    pub fn save(&self) {
        let p = settings_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, json!({"mic": self.mic, "auto_stop": self.auto_stop, "cleanup": self.cleanup.name()}).to_string());
    }
}

const DEFAULT_WORDS: &str = "# Names and words VoiceType should spell your way, one per line.
# Whisper gets this list as a hint, so it writes \"Claude\" instead of \"cloud\".
#
# If a word still comes out wrong, add a fix:  what it writes => what you want
# Example:  cloud code => Claude Code

Claude
Claude Code
Anthropic
Codex
ChatGPT
GitHub
WhatsApp
VoiceType
Rust
Kotlin
Supabase
";

pub fn words_file() -> PathBuf {
    let p = settings_path().with_file_name("words.txt");
    if !p.exists() {
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&p, DEFAULT_WORDS);
    }
    p
}

#[derive(Default)]
pub struct Words {
    pub prompt: Option<String>,
    pub fixes: Vec<(String, String)>,
}

pub fn words() -> Words {
    let raw = std::fs::read_to_string(words_file()).unwrap_or_default();
    let mut names = Vec::new();
    let mut fixes = Vec::new();
    for line in raw.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')) {
        match line.split_once("=>") {
            Some((from, to)) if !from.trim().is_empty() => fixes.push((from.trim().to_lowercase(), to.trim().to_string())),
            _ => names.push(line.to_string()),
        }
    }
    names.truncate(80);
    Words { prompt: (!names.is_empty()).then(|| format!("{}.", names.join(", "))), fixes }
}

pub fn apply_fixes(text: &str, fixes: &[(String, String)]) -> String {
    let mut out = text.to_string();
    for (from, to) in fixes {
        if from.is_empty() {
            continue;
        }
        let lower = out.to_lowercase();
        // Lowercasing can change byte lengths (İ → i + combining dot).
        let mut boundaries = vec![(0, 0)];
        let mut folded = 0;
        for (pos, c) in out.char_indices() {
            folded += c.to_lowercase().map(char::len_utf8).sum::<usize>();
            boundaries.push((folded, pos + c.len_utf8()));
        }
        let mut next = String::new();
        let mut copied = 0;
        for (pos, _) in lower.match_indices(from.as_str()) {
            let (Ok(start), Ok(end)) = (
                boundaries.binary_search_by_key(&pos, |&(folded, _)| folded),
                boundaries.binary_search_by_key(&(pos + from.len()), |&(folded, _)| folded),
            ) else { continue };
            let (start, end) = (boundaries[start].1, boundaries[end].1);
            let before = out[..start].chars().next_back().is_some_and(char::is_alphanumeric);
            let after = out[end..].chars().next().is_some_and(char::is_alphanumeric);
            if before || after {
                continue;
            }
            next.push_str(&out[copied..start]);
            next.push_str(to);
            copied = end;
        }
        next.push_str(&out[copied..]);
        out = next;
    }
    out
}

pub fn copy(text: &str) {
    let _ = Command::new("wl-copy").arg("--").arg(text).status();
}

fn ago(t: u64) -> String {
    let d = now().saturating_sub(t);
    match d {
        0..60 => "just now".into(),
        60..3600 => format!("{} min ago", d / 60),
        3600..86400 => format!("{} h ago", d / 3600),
        86400..172800 => "yesterday".into(),
        _ => format!("{} days ago", d / 86400),
    }
}

struct HistoryApp {
    entries: Vec<Entry>,
    query: String,
    copied: Option<usize>,
}

impl eframe::App for HistoryApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Frame::new().inner_margin(14.0).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut self.query).hint_text("Search").desired_width(ui.available_width() - 90.0));
                if ui.button("Refresh").clicked() {
                    self.entries = load();
                    self.copied = None;
                }
            });
            ui.add_space(4.0);
            let q = self.query.to_lowercase();
            let shown: Vec<usize> = (0..self.entries.len()).rev().filter(|&i| q.is_empty() || self.entries[i].text.to_lowercase().contains(&q)).collect();
            ui.label(egui::RichText::new(format!("{} dictations", shown.len())).weak());
            ui.label(egui::RichText::new("Edit text before copying or typing. Edits last until Refresh or closing this window.").small().weak());
            ui.add_space(6.0);
            if shown.is_empty() {
                ui.label(if self.entries.is_empty() { "Nothing yet. Press Meta+Space and speak." } else { "No matches." });
                return;
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for i in shown {
                    let e = &mut self.entries[i];
                    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{} · {:.0}s", ago(e.t), e.secs)).small().weak());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.add_enabled(!e.text.trim().is_empty(), egui::Button::new("Type")).on_hover_text("Close this window and type it into the window you were using").clicked() {
                                    let text = e.text.clone();
                                    std::thread::spawn(move || crate::send(&format!("typelater {text}")));
                                    std::thread::sleep(std::time::Duration::from_millis(80));
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                let label = if self.copied == Some(i) { "Copied" } else { "Copy" };
                                if ui.add_enabled(!e.text.trim().is_empty(), egui::Button::new(label)).clicked() {
                                    copy(&e.text);
                                    self.copied = Some(i);
                                }
                            });
                        });
                        if ui.add(egui::TextEdit::multiline(&mut e.text).font(egui::FontId::proportional(15.0)).hint_text("Dictation text").desired_width(f32::INFINITY).desired_rows(2)).changed() {
                            self.copied = None;
                        }
                        if let Some(raw) = &e.raw {
                            ui.label(egui::RichText::new(format!("Before cleanup: {raw}")).small().weak());
                        }
                    });
                    ui.add_space(6.0);
                }
            });
        });
    }
}

pub fn window() -> Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Voice typing history")
            .with_app_id("voicetype-history")
            .with_inner_size([560.0, 640.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Voice typing history",
        options,
        Box::new(|_| Ok(Box::new(HistoryApp { entries: load(), query: String::new(), copied: None }))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spelling_fixes_work_in_non_ascii_text() {
        let fixes = vec![("cloud code".into(), "Claude Code".into()), ("cafe".into(), "café".into())];
        assert_eq!(apply_fixes("İ use CLOUD CODE, CAFE!", &fixes), "İ use Claude Code, café!");
        assert_eq!(apply_fixes("cloud coder / cloud code / cloud code", &fixes), "cloud coder / Claude Code / Claude Code");
    }

    #[test]
    fn spelling_fixes_do_not_split_expanded_characters_or_repeat_replacements() {
        assert_eq!(apply_fixes("İ", &[("i".into(), "x".into())]), "İ");
        assert_eq!(apply_fixes("İ", &[("i\u{307}".into(), "I".into())]), "I");
        assert_eq!(apply_fixes("code code", &[("code".into(), "code code".into())]), "code code code code");
        assert_eq!(apply_fixes("code", &[(String::new(), "x".into())]), "code");
    }
}
