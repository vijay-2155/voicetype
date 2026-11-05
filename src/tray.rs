use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use crate::cleanup::{MODES, Mode};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording,
    Busy,
}

pub struct VoiceTray {
    pub state: State,
    pub mic: Option<String>,
    pub mics: Vec<(String, String)>,
    pub recent: Vec<String>,
    pub auto_stop: f32,
    pub cleanup: Mode,
}

const AUTO_STOP: [f32; 5] = [0.0, 1.0, 1.5, 2.0, 3.0];

fn send_async(cmd: String) {
    std::thread::spawn(move || crate::send(&cmd));
}

fn short(text: &str) -> String {
    let t: String = text.chars().take(48).collect();
    if t.len() < text.len() { format!("{t}…") } else { t }
}

pub fn list_mics() -> Vec<(String, String)> {
    let Ok(out) = std::process::Command::new("pactl").args(["-f", "json", "list", "sources"]).output() else { return Vec::new() };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(&out.stdout) else { return Vec::new() };
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|s| Some((s["name"].as_str()?.to_string(), mic_label(s)?)))
        .filter(|(n, _)| !n.ends_with(".monitor"))
        .collect()
}

fn mic_label(source: &serde_json::Value) -> Option<String> {
    let description = source["description"].as_str()?;
    let port = source["ports"].as_array().and_then(|ports| {
        ports.iter().find(|p| p["name"] == source["active_port"])
    }).and_then(|p| p["description"].as_str());
    Some(match port {
        Some(port) => format!("{description} — {port}"),
        None => description.to_string(),
    })
}

impl ksni::Tray for VoiceTray {
    fn id(&self) -> String {
        "voicetype".into()
    }

    fn title(&self) -> String {
        "Voice typing".into()
    }

    fn icon_name(&self) -> String {
        match self.state {
            State::Idle => "mic-ready",
            State::Recording => "media-record",
            State::Busy => "view-refresh",
        }
        .into()
    }

    fn status(&self) -> ksni::Status {
        match self.state {
            State::Recording => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    fn attention_icon_name(&self) -> String {
        "media-record".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.state {
            State::Idle => "Meta+Space to dictate".to_string(),
            State::Recording => "Listening".into(),
            State::Busy => "Transcribing…".into(),
        };
        ksni::ToolTip { title: "Voice typing".into(), description, ..Default::default() }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        send_async("toggle".into());
    }

    fn menu_about_to_show(&mut self) {
        self.mics = list_mics();
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        let status = match self.state {
            State::Idle => "Ready".to_string(),
            State::Recording => "Listening".into(),
            State::Busy => "Transcribing…".into(),
        };
        let mut mic_options = vec![RadioItem { label: "System default".into(), ..Default::default() }];
        mic_options.extend(self.mics.iter().map(|(_, d)| RadioItem { label: d.clone(), ..Default::default() }));
        let mic_selected = match &self.mic {
            None => 0,
            Some(mic) => self.mics.iter().position(|(name, _)| name == mic).map_or_else(|| {
                mic_options.push(RadioItem { label: "Selected microphone unavailable — choose another".into(), enabled: false, ..Default::default() });
                mic_options.len() - 1
            }, |i| i + 1),
        };

        let mut recent: Vec<MenuItem<Self>> = self
            .recent
            .iter()
            .rev()
            .map(|text| {
                let text = text.clone();
                StandardItem {
                    label: short(&text),
                    icon_name: "edit-copy".into(),
                    activate: Box::new(move |_| crate::history::copy(&text)),
                    ..Default::default()
                }
                .into()
            })
            .collect();
        if recent.is_empty() {
            recent.push(StandardItem { label: "Nothing yet".into(), enabled: false, ..Default::default() }.into());
        }

        vec![
            StandardItem { label: status, enabled: false, ..Default::default() }.into(),
            StandardItem {
                label: if self.state == State::Recording { "Stop and type" } else { "Start dictation" }.into(),
                icon_name: if self.state == State::Recording { "media-playback-stop" } else { "media-record" }.into(),
                enabled: self.state != State::Busy,
                activate: Box::new(|_| send_async("toggle".into())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Cancel dictation".into(),
                icon_name: "process-stop".into(),
                enabled: self.state == State::Recording,
                activate: Box::new(|_| send_async("cancel".into())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Microphone".into(),
                icon_name: "audio-input-microphone".into(),
                submenu: vec![
                    RadioGroup {
                        selected: mic_selected,
                        select: Box::new(|this: &mut Self, i| {
                            this.mic = if i == 0 { None } else { this.mics.get(i - 1).map(|m| m.0.clone()) };
                            send_async(format!("mic {}", this.mic.as_deref().unwrap_or("default")));
                        }),
                        options: mic_options,
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Auto-stop after pause".into(),
                icon_name: "chronometer".into(),
                submenu: vec![
                    RadioGroup {
                        selected: AUTO_STOP.iter().position(|s| (*s - self.auto_stop).abs() < 0.01).unwrap_or(3),
                        select: Box::new(|this: &mut Self, i| {
                            this.auto_stop = AUTO_STOP[i];
                            send_async(format!("autostop {}", AUTO_STOP[i]));
                        }),
                        options: AUTO_STOP
                            .iter()
                            .map(|s| RadioItem {
                                label: if *s == 0.0 { "Off (press Meta+Space to stop)".into() } else { format!("{s} seconds") },
                                ..Default::default()
                            })
                            .collect(),
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            SubMenu {
                label: "Text cleanup".into(),
                icon_name: "tools-check-spelling".into(),
                submenu: vec![
                    RadioGroup {
                        selected: MODES.iter().position(|m| *m == self.cleanup).unwrap_or(0),
                        select: Box::new(|this: &mut Self, i| {
                            this.cleanup = MODES[i];
                            send_async(format!("cleanup {}", MODES[i].name()));
                        }),
                        options: vec![
                            RadioItem { label: "Off: type exactly what was heard".into(), ..Default::default() },
                            RadioItem { label: "Grammar: light fixes".into(), ..Default::default() },
                            RadioItem { label: "Prompt: clear rewrite for AI tools".into(), ..Default::default() },
                        ],
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu { label: "Recent (click to copy)".into(), icon_name: "document-open-recent".into(), submenu: recent, ..Default::default() }.into(),
            StandardItem {
                label: "Type last again".into(),
                icon_name: "edit-redo".into(),
                enabled: !self.recent.is_empty(),
                activate: Box::new(|_| send_async("retype".into())),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "Edit words and names…".into(),
                icon_name: "document-edit".into(),
                activate: Box::new(|_| {
                    if let Ok(mut child) = std::process::Command::new("xdg-open").arg(crate::history::words_file()).spawn() {
                        std::thread::spawn(move || child.wait());
                    }
                }),
                ..Default::default()
            }
            .into(),
            StandardItem {
                label: "History…".into(),
                icon_name: "view-history".into(),
                activate: Box::new(|_| {
                    if let Ok(exe) = std::env::current_exe() {
                        if let Ok(mut child) = std::process::Command::new(exe).arg("history").spawn() {
                            std::thread::spawn(move || child.wait());
                        }
                    }
                }),
                ..Default::default()
            }
            .into(),
        ]
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn missing_microphone_is_not_shown_as_system_default() {
        use ksni::Tray;
        for (mic, expected) in [(None, 0), (Some("present"), 1), (Some("missing"), 2)] {
            let tray = VoiceTray {
                state: State::Idle, mic: mic.map(str::to_string),
                mics: vec![("present".into(), "Internal Microphone".into())],
                recent: Vec::new(), auto_stop: 3.0, cleanup: Mode::Off,
            };
            let menu = tray.menu();
            let group = menu.iter().find_map(|item| match item {
                MenuItem::SubMenu(menu) if menu.label == "Microphone" => match &menu.submenu[0] {
                    MenuItem::RadioGroup(group) => Some(group),
                    _ => None,
                },
                _ => None,
            }).unwrap();
            assert_eq!(group.selected, expected);
            assert_eq!(group.options[expected].enabled, expected != 2);
        }
    }

    #[test]
    fn microphone_labels_show_the_active_input_port() {
        let source = json!({"description": "Built-in Audio", "active_port": "internal", "ports": [
            {"name": "headset", "description": "Headset"},
            {"name": "internal", "description": "Internal Microphone"}
        ]});
        assert_eq!(mic_label(&source).as_deref(), Some("Built-in Audio — Internal Microphone"));
        assert_eq!(mic_label(&json!({"description": "USB Mic"})).as_deref(), Some("USB Mic"));
        assert_eq!(mic_label(&json!({})), None);
    }
}
