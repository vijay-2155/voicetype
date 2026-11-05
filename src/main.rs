mod audio;
mod cleanup;
mod engine;
mod history;
mod output;
mod overlay;
mod tray;

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use smithay_client_toolkit::reexports::calloop::channel;

use audio::Recorder;
use engine::{Engine, RATE};
use overlay::UiMsg;
use tray::{State, VoiceTray};

type Tray = Option<ksni::blocking::Handle<VoiceTray>>;

fn set_tray(tray: &Tray, f: impl FnOnce(&mut VoiceTray)) {
    if let Some(h) = tray {
        h.update(f);
    }
}

fn runtime_dir() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/tmp"))
}

fn sock_path() -> PathBuf {
    runtime_dir().join("voicetype.sock")
}

type Spec = Arc<Mutex<Option<(usize, String, String)>>>;

struct Session {
    id: u64,
    rec: Recorder,
    stop: Arc<AtomicBool>,
    spec: Spec,
    cleanup: cleanup::Mode,
}

struct Ctl {
    engine: Arc<Mutex<Engine>>,
    ui: channel::Sender<UiMsg>,
    session: Option<Session>,
    busy: Arc<AtomicBool>,
    settings: history::Settings,
    tray: Tray,
    next_id: u64,
}

impl Ctl {
    fn command(&mut self, line: &str) -> String {
        let mut parts = line.split_whitespace();
        match parts.next().unwrap_or("") {
            "toggle" => {
                if self.session.is_some() {
                    self.finish();
                    "stopped".into()
                } else if self.busy.load(Ordering::SeqCst) {
                    "busy".into()
                } else {
                    match self.start() {
                        Ok(()) => "recording".into(),
                        Err(e) => {
                            let _ = self.ui.send(UiMsg::Error(format!("Mic failed: {e}")));
                            format!("error: {e}")
                        }
                    }
                }
            }
            "cancel" => match self.session.take() {
                Some(s) => {
                    s.stop.store(true, Ordering::SeqCst);
                    s.rec.cancel();
                    let _ = self.ui.send(UiMsg::Hide);
                    set_tray(&self.tray, |t| t.state = State::Idle);
                    "cancelled".into()
                }
                None => "idle".into(),
            },
            "status" => if self.session.is_some() {
                "recording"
            } else if self.busy.load(Ordering::SeqCst) {
                "busy"
            } else {
                "idle"
            }
            .into(),
            "stop" => {
                let id = parts.next().and_then(|n| n.parse::<u64>().ok());
                if self.session.as_ref().is_some_and(|s| id.is_none_or(|id| id == s.id)) {
                    self.finish();
                    "stopped".into()
                } else {
                    "idle".into()
                }
            }
            "autostop" => match parts.next().and_then(|n| n.parse::<f32>().ok()) {
                Some(secs) => {
                    self.settings.auto_stop = secs.max(0.0);
                    self.settings.save();
                    set_tray(&self.tray, |t| t.auto_stop = secs.max(0.0));
                    format!("autostop {secs}")
                }
                None => "usage: autostop <seconds> (0 = off)".into(),
            },
            "cleanup" => match parts.next().and_then(cleanup::Mode::parse) {
                Some(mode) => {
                    self.settings.cleanup = mode;
                    self.settings.save();
                    set_tray(&self.tray, |t| t.cleanup = mode);
                    if mode != cleanup::Mode::Off {
                        std::thread::spawn(cleanup::warm);
                    }
                    format!("cleanup {}", mode.name())
                }
                None => "usage: cleanup off|grammar|prompt".into(),
            },
            "mic" => {
                let mic = parts.next().filter(|m| *m != "default").map(str::to_string);
                self.settings.mic = mic.clone();
                self.settings.save();
                set_tray(&self.tray, |t| t.mic = mic);
                "ok".into()
            }
            "retype" => match history::load().pop() {
                Some(e) => {
                    std::thread::spawn(move || output::type_text(&format!("{} ", e.text)));
                    "typing".into()
                }
                None => "nothing to type".into(),
            },
            "typelater" => {
                let text = parts.collect::<Vec<_>>().join(" ");
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(500));
                    output::type_text(&format!("{text} "));
                });
                "typing".into()
            }
            _ => "unknown command".into(),
        }
    }

    fn start(&mut self) -> Result<()> {
        let ui = self.ui.clone();
        let rec = Recorder::start(self.settings.mic.as_deref(), move |l| {
            let _ = ui.send(UiMsg::Level(l));
        })?;
        let _ = self.ui.send(UiMsg::Listening);
        let tidy = self.settings.cleanup;
        if tidy != cleanup::Mode::Off {
            std::thread::spawn(cleanup::warm);
        }
        let stop = Arc::new(AtomicBool::new(false));
        let samples = rec.samples.clone();
        let vad = rec.vad.clone();
        let engine = self.engine.clone();
        self.next_id += 1;
        let id = self.next_id;
        {
            let (vad, ui, flag, wait) = (rec.vad.clone(), self.ui.clone(), stop.clone(), self.settings.auto_stop);
            let health = rec.health.clone();
            std::thread::spawn(move || watch_pause(id, wait, &vad, &ui, &flag, &health));
        }
        let spec: Spec = Arc::new(Mutex::new(None));
        let flag = stop.clone();
        let spec_w = spec.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(100));
                let (heard, quiet, vlen) = {
                    let v = vad.lock().unwrap();
                    (audio::heard_speech(&v), audio::trailing_silence(&v), v.len())
                };
                if !heard {
                    continue;
                }
                if quiet >= SPEC_AFTER {
                    let fresh = spec_w.lock().unwrap().as_ref().is_none_or(|(l, ..)| *l < vlen.saturating_sub((quiet * 100.0) as usize));
                    if !fresh {
                        continue;
                    }
                    let (clip, l) = {
                        let s = samples.lock().unwrap();
                        let v = vad.lock().unwrap();
                        (audio::finish_clip(&s, &v), v.len())
                    };
                    // Hold the engine until the result is stored, so finish() waits and can use it.
                    let mut eng = engine.lock().unwrap();
                    if let Ok(raw) = eng.transcribe(&clip) {
                        let text = cleanup::apply(tidy, &raw);
                        *spec_w.lock().unwrap() = Some((l, raw, text));
                    }
                }
            }
        });
        self.session = Some(Session { id, rec, stop, spec, cleanup: tidy });
        set_tray(&self.tray, |t| t.state = State::Recording);
        eprintln!("recording");
        Ok(())
    }

    fn finish(&mut self) {
        let Some(s) = self.session.take() else { return };
        s.stop.store(true, Ordering::SeqCst);
        self.busy.store(true, Ordering::SeqCst);
        let _ = self.ui.send(UiMsg::Transcribing);
        set_tray(&self.tray, |t| t.state = State::Busy);
        let tray = self.tray.clone();
        let engine = self.engine.clone();
        let ui = self.ui.clone();
        let busy = self.busy.clone();
        std::thread::spawn(move || {
            let t = Instant::now();
            let (audio, vad, capture_failed) = s.rec.stop();
            let _ = audio::write_wav(&runtime_dir().join("voicetype-last.wav"), &audio);
            let result = if audio.is_empty() {
                Ok((String::new(), String::new()))
            } else {
                let mut eng = engine.lock().unwrap();
                let early = s.spec.lock().unwrap().take().filter(|(l, ..)| !audio::speech_after(&vad, *l));
                match early {
                    Some((_, raw, text)) => {
                        eprintln!("used the transcript made during the pause");
                        Ok((raw, text))
                    }
                    None => eng.transcribe(&audio).map(|raw| {
                        let text = cleanup::apply(s.cleanup, &raw);
                        (raw, text)
                    }),
                }
            };
            match result {
                Ok((raw, text)) if !text.is_empty() => {
                    eprintln!("{:.1}s audio -> {:.2}s: {text}", audio.len() as f32 / RATE as f32, t.elapsed().as_secs_f32());
                    let saved = history::append(audio.len() as f32 / RATE as f32, &text, &raw);
                    if let Err(e) = &saved {
                        eprintln!("could not save history: {e}");
                    }
                    if capture_failed {
                        let message = if saved.is_ok() {
                            "Microphone stopped. Captured text saved in History — check your mic."
                        } else {
                            "Microphone stopped; history could not be saved. Copy captured text from the tray’s Recent menu."
                        };
                        let _ = ui.send(UiMsg::Error(message.into()));
                    } else {
                        let _ = ui.send(UiMsg::Done(text.clone()));
                        output::type_text(&format!("{text} "));
                    }
                    set_tray(&tray, |t| {
                        t.recent.push(text.clone());
                        if t.recent.len() > 10 {
                            t.recent.remove(0);
                        }
                    });
                }
                Ok(_) => {
                    eprintln!("{:.1}s audio -> nothing heard", audio.len() as f32 / RATE as f32);
                    let _ = ui.send(UiMsg::Error(if capture_failed { "Microphone stopped — check its connection and select it in the tray" } else { "No speech detected — check mute, move closer, or choose another microphone" }.into()));
                }
                Err(e) => {
                    eprintln!("transcribe failed: {e:#}");
                    let _ = ui.send(UiMsg::Error(format!("{e}")));
                }
            }
            busy.store(false, Ordering::SeqCst);
            set_tray(&tray, |t| t.state = State::Idle);
        });
    }
}

const GRACE: f32 = 0.5;
// Wait for finish_clip's 450 ms trailing margin before caching the final words.
const SPEC_AFTER: f32 = 0.5;
const NO_SPEECH_LIMIT: Duration = Duration::from_secs(10);

fn watch_pause(id: u64, wait: f32, vad: &Mutex<Vec<f32>>, ui: &channel::Sender<UiMsg>, flag: &AtomicBool, health: &Mutex<audio::CaptureHealth>) {
    let began = Instant::now();
    let mut shown = 0.0f32;
    while !flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
        if flag.load(Ordering::SeqCst) {
            return;
        }
        if health.lock().unwrap().failed() {
            send(&format!("stop {id}"));
            return;
        }
        let (heard, quiet) = {
            let v = vad.lock().unwrap();
            (audio::heard_speech(&v), audio::trailing_silence(&v))
        };
        if !heard {
            if began.elapsed() > NO_SPEECH_LIMIT {
                send(&format!("stop {id}"));
                return;
            }
            continue;
        }
        if wait <= 0.0 {
            continue;
        }
        let progress = ((quiet - GRACE) / (wait - GRACE).max(0.1)).clamp(0.0, 1.0);
        if (progress - shown).abs() > 0.02 || (progress == 0.0 && shown != 0.0) {
            shown = progress;
            let _ = ui.send(UiMsg::Pause(progress));
        }
        if quiet >= wait {
            send(&format!("stop {id}"));
            return;
        }
    }
}

fn daemon() -> Result<()> {
    let engine = Arc::new(Mutex::new(Engine::new()));
    {
        let engine = engine.clone();
        std::thread::spawn(move || {
            if let Err(e) = engine.lock().unwrap().preload() {
                eprintln!("preload failed: {e:#}");
            }
        });
    }
    let (tx, rx) = channel::channel();
    let settings = history::Settings::load();
    let recent = history::load().into_iter().rev().take(10).rev().map(|e| e.text).collect();
    let tray = {
        use ksni::blocking::TrayMethods;
        let t = VoiceTray {
            state: State::Idle,
            mic: settings.mic.clone(),
            mics: tray::list_mics(),
            recent,
            auto_stop: settings.auto_stop,
            cleanup: settings.cleanup,
        };
        match t.spawn() {
            Ok(h) => Some(h),
            Err(e) => {
                eprintln!("tray unavailable: {e}");
                None
            }
        }
    };
    let ctl = Arc::new(Mutex::new(Ctl { engine, ui: tx, session: None, busy: Arc::new(AtomicBool::new(false)), settings, tray, next_id: 0 }));

    let path = sock_path();
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).with_context(|| format!("binding {}", path.display()))?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let mut line = String::new();
            let mut reader = BufReader::new(&conn);
            if reader.read_line(&mut line).is_err() {
                continue;
            }
            let reply = ctl.lock().unwrap().command(line.trim());
            let _ = (&conn).write_all(reply.as_bytes());
        }
    });
    eprintln!("ready on {}", path.display());
    overlay::run(rx)
}

pub fn send(cmd: &str) -> Option<String> {
    let mut s = UnixStream::connect(sock_path()).ok()?;
    s.write_all(format!("{}\n", cmd.replace('\n', " ")).as_bytes()).ok()?;
    s.shutdown(std::net::Shutdown::Write).ok()?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply).ok()?;
    Some(reply)
}

fn client(args: &[String]) -> Result<()> {
    let cmd = if args.is_empty() { "toggle".to_string() } else { args.join(" ") };
    let mut s = match UnixStream::connect(sock_path()) {
        Ok(s) => s,
        Err(_) => {
            let _ = std::process::Command::new("notify-send")
                .args(["-a", "Voice typing", "Voice typing is not running", "systemctl --user start voicetype"])
                .status();
            anyhow::bail!("daemon not running");
        }
    };
    s.write_all(format!("{cmd}\n").as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    BufReader::new(s).read_line(&mut reply)?;
    println!("{reply}");
    Ok(())
}

fn files(args: &[String]) -> Result<()> {
    let mut engine = Engine::new();
    for f in args {
        let audio = audio::decode_file(f, std::env::var_os("VOICETYPE_RAW").is_none())?;
        let t = Instant::now();
        let text = engine.transcribe(&audio)?;
        eprintln!("-- {f} ({:.1}s audio, {:.2}s)", audio.len() as f32 / RATE as f32, t.elapsed().as_secs_f32());
        println!("{text}");
    }
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("daemon") => daemon(),
        Some("file") => files(&args[1..]),
        Some("history") => history::window(),
        Some("clean") => match args.get(1).and_then(|m| cleanup::Mode::parse(m)) {
            Some(mode) => {
                cleanup::warm();
                println!("{}", cleanup::apply(mode, &args[2..].join(" ")));
                Ok(())
            }
            None => anyhow::bail!("usage: voicetype clean grammar|prompt \"some text\""),
        },
        _ => client(&args),
    }
}
