use ksni::menu::{MenuItem, RadioGroup, RadioItem, StandardItem, SubMenu};

use crate::engine::Profile;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum State {
    Idle,
    Recording(Profile),
    Busy,
}

pub struct VoiceTray {
    pub state: State,
    pub profile: Profile,
    pub mic: Option<String>,
    pub mics: Vec<(String, String)>,
    pub recent: Vec<String>,
    pub auto_stop: f32,
}

const AUTO_STOP: [f32; 5] = [0.0, 1.0, 1.5, 2.0, 3.0];

const PROFILES: [Profile; 3] = [Profile::Auto, Profile::En, Profile::Te];

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
        .filter_map(|s| Some((s["name"].as_str()?.to_string(), s["description"].as_str()?.to_string())))
        .filter(|(n, _)| !n.ends_with(".monitor"))
        .collect()
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
            State::Recording(_) => "media-record",
            State::Busy => "view-refresh",
        }
        .into()
    }

    fn status(&self) -> ksni::Status {
        match self.state {
            State::Recording(_) => ksni::Status::NeedsAttention,
            _ => ksni::Status::Active,
        }
    }

    fn attention_icon_name(&self) -> String {
        "media-record".into()
    }

    fn tool_tip(&self) -> ksni::ToolTip {
        let description = match self.state {
            State::Idle => format!("{} · Meta+Space to dictate", self.profile.label()),
            State::Recording(p) => format!("Listening ({})", p.label()),
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
            State::Recording(p) => format!("Listening ({})", p.label()),
            State::Busy => "Transcribing…".into(),
        };
        let mut mic_options = vec![RadioItem { label: "System default".into(), ..Default::default() }];
        mic_options.extend(self.mics.iter().map(|(_, d)| RadioItem { label: d.clone(), ..Default::default() }));
        let mic_selected = self.mic.as_ref().and_then(|m| self.mics.iter().position(|(n, _)| n == m)).map_or(0, |i| i + 1);

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
                label: if matches!(self.state, State::Recording(_)) { "Stop and type" } else { "Start dictation" }.into(),
                icon_name: if matches!(self.state, State::Recording(_)) { "media-playback-stop" } else { "media-record" }.into(),
                enabled: self.state != State::Busy,
                activate: Box::new(|_| send_async("toggle".into())),
                ..Default::default()
            }
            .into(),
            MenuItem::Separator,
            SubMenu {
                label: "Language".into(),
                icon_name: "languages".into(),
                submenu: vec![
                    RadioGroup {
                        selected: PROFILES.iter().position(|p| *p == self.profile).unwrap_or(0),
                        select: Box::new(|this: &mut Self, i| {
                            this.profile = PROFILES[i];
                            send_async(format!("mode {}", PROFILES[i].name()));
                        }),
                        options: vec![
                            RadioItem { label: "Auto detect".into(), ..Default::default() },
                            RadioItem { label: "English".into(), ..Default::default() },
                            RadioItem { label: "Telugu (తెలుగు)".into(), ..Default::default() },
                        ],
                    }
                    .into(),
                ],
                ..Default::default()
            }
            .into(),
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
