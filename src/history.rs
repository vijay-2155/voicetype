use std::io::Write;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use eframe::egui;
use serde_json::{Value, json};

pub struct Entry {
    pub t: u64,
    pub profile: String,
    pub secs: f32,
    pub text: String,
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

pub fn append(profile: &str, secs: f32, text: &str) {
    let p = path();
    if let Some(dir) = p.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let line = json!({"t": now(), "p": profile, "s": (secs * 10.0).round() / 10.0, "text": text});
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&p) {
        let _ = writeln!(f, "{line}");
    }
}

pub fn load() -> Vec<Entry> {
    let Ok(raw) = std::fs::read_to_string(path()) else { return Vec::new() };
    raw.lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .map(|v| Entry {
            t: v["t"].as_u64().unwrap_or(0),
            profile: v["p"].as_str().unwrap_or("").to_string(),
            secs: v["s"].as_f64().unwrap_or(0.0) as f32,
            text: v["text"].as_str().unwrap_or("").to_string(),
        })
        .filter(|e| !e.text.is_empty())
        .collect()
}

pub struct Settings {
    pub profile: String,
    pub mic: Option<String>,
    pub auto_stop: f32,
    pub languages: Vec<String>,
}

impl Settings {
    pub fn load() -> Self {
        let v: Value = std::fs::read_to_string(settings_path()).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or(Value::Null);
        Self {
            profile: v["profile"].as_str().unwrap_or("auto").to_string(),
            mic: v["mic"].as_str().map(str::to_string),
            auto_stop: v["auto_stop"].as_f64().unwrap_or(2.0) as f32,
            languages: v["languages"]
                .as_array()
                .map(|a| a.iter().filter_map(|l| l.as_str().map(str::to_string)).collect::<Vec<_>>())
                .filter(|l| !l.is_empty())
                .unwrap_or_else(|| vec!["en".into(), "te".into()]),
        }
    }

    pub fn save(&self) {
        let p = settings_path();
        if let Some(dir) = p.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(p, json!({"profile": self.profile, "mic": self.mic, "auto_stop": self.auto_stop, "languages": self.languages}).to_string());
    }
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

fn profile_label(p: &str) -> &str {
    match p {
        "te" => "తెలుగు",
        "en" => "English",
        _ => "Auto",
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
            ui.add_space(6.0);
            if shown.is_empty() {
                ui.label(if self.entries.is_empty() { "Nothing yet. Press Meta+Space and speak." } else { "No matches." });
                return;
            }
            egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
                for i in shown {
                    let e = &self.entries[i];
                    egui::Frame::group(ui.style()).inner_margin(10.0).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(format!("{} · {} · {:.0}s", ago(e.t), profile_label(&e.profile), e.secs)).small().weak());
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if ui.button("Type").on_hover_text("Close this window and type it into the window you were using").clicked() {
                                    let text = e.text.clone();
                                    std::thread::spawn(move || crate::send(&format!("typelater {text}")));
                                    std::thread::sleep(std::time::Duration::from_millis(80));
                                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                                }
                                let label = if self.copied == Some(i) { "Copied" } else { "Copy" };
                                if ui.button(label).clicked() {
                                    copy(&e.text);
                                    self.copied = Some(i);
                                }
                            });
                        });
                        ui.add(egui::Label::new(egui::RichText::new(&e.text).size(15.0)).wrap());
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
