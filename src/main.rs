mod audio;
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
use engine::{Engine, Profile, RATE};
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

struct Session {
    id: u64,
    rec: Recorder,
    profile: Profile,
    stop: Arc<AtomicBool>,
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
                let profile = parts.next().and_then(Profile::parse).unwrap_or_else(|| self.profile());
                if self.session.is_some() {
                    self.finish();
                    "stopped".into()
                } else if self.busy.load(Ordering::SeqCst) {
                    "busy".into()
                } else {
                    match self.start(profile) {
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
            "mode" => match parts.next().and_then(Profile::parse) {
                Some(p) => {
                    self.settings.profile = p.name().into();
                    self.settings.save();
                    set_tray(&self.tray, |t| t.profile = p);
                    format!("mode {}", p.name())
                }
                None => "usage: mode auto|en|te".into(),
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

    fn profile(&self) -> Profile {
        Profile::parse(&self.settings.profile).unwrap_or(Profile::Auto)
    }

    fn start(&mut self, profile: Profile) -> Result<()> {
        let ui = self.ui.clone();
        let rec = Recorder::start(self.settings.mic.as_deref(), move |l| {
            let _ = ui.send(UiMsg::Level(l));
        })?;
        let _ = self.ui.send(UiMsg::Listening(profile.label()));
        let stop = Arc::new(AtomicBool::new(false));
        let samples = rec.samples.clone();
        let vad = rec.vad.clone();
        let engine = self.engine.clone();
        let ui = self.ui.clone();
        self.next_id += 1;
        let id = self.next_id;
        {
            let (vad, ui, flag, wait) = (rec.vad.clone(), self.ui.clone(), stop.clone(), self.settings.auto_stop);
            std::thread::spawn(move || watch_pause(id, wait, &vad, &ui, &flag));
        }
        let flag = stop.clone();
        std::thread::spawn(move || {
            let mut last = 0;
            while !flag.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(500));
                if !audio::heard_speech(&vad.lock().unwrap()) {
                    continue;
                }
                let snap = {
                    let s = samples.lock().unwrap();
                    if s.len() < RATE * 3 / 4 || s.len() - last < RATE / 3 {
                        continue;
                    }
                    last = s.len();
                    s[s.len().saturating_sub(RATE * 25)..].to_vec()
                };
                let Ok(mut eng) = engine.try_lock() else { continue };
                if flag.load(Ordering::SeqCst) {
                    break;
                }
                if let Ok(text) = eng.transcribe(profile, &snap, true) {
                    if !flag.load(Ordering::SeqCst) && !text.is_empty() {
                        let _ = ui.send(UiMsg::Partial(text));
                    }
                }
            }
        });
        self.session = Some(Session { id, rec, profile, stop });
        set_tray(&self.tray, |t| t.state = State::Recording(profile));
        eprintln!("recording [{}]", profile.label());
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
            let audio = s.rec.stop();
            let _ = audio::write_wav(&runtime_dir().join("voicetype-last.wav"), &audio);
            let t = Instant::now();
            let result = if audio.is_empty() { Ok(String::new()) } else { engine.lock().unwrap().transcribe(s.profile, &audio, false) };
            match result {
                Ok(text) if !text.is_empty() => {
                    eprintln!("{:.1}s audio -> {:.2}s: {text}", audio.len() as f32 / RATE as f32, t.elapsed().as_secs_f32());
                    let _ = ui.send(UiMsg::Done(text.clone()));
                    output::type_text(&format!("{text} "));
                    history::append(s.profile.name(), audio.len() as f32 / RATE as f32, &text);
                    set_tray(&tray, |t| {
                        t.recent.push(text.clone());
                        if t.recent.len() > 10 {
                            t.recent.remove(0);
                        }
                    });
                }
                Ok(_) => {
                    eprintln!("{:.1}s audio -> nothing heard", audio.len() as f32 / RATE as f32);
                    let _ = ui.send(UiMsg::Error("Didn't catch that — try again".into()));
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
const NO_SPEECH_LIMIT: Duration = Duration::from_secs(10);

fn watch_pause(id: u64, wait: f32, vad: &Mutex<Vec<f32>>, ui: &channel::Sender<UiMsg>, flag: &AtomicBool) {
    let began = Instant::now();
    let mut shown = 0.0f32;
    while !flag.load(Ordering::SeqCst) {
        std::thread::sleep(Duration::from_millis(50));
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
            if let Err(e) = engine.lock().unwrap().preload(Profile::Auto) {
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
            profile: Profile::parse(&settings.profile).unwrap_or(Profile::Auto),
            mic: settings.mic.clone(),
            mics: tray::list_mics(),
            recent,
            auto_stop: settings.auto_stop,
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
    let (profile, rest) = match args.first().and_then(|a| Profile::parse(a)) {
        Some(p) => (p, &args[1..]),
        None => (Profile::Auto, args),
    };
    let mut engine = Engine::new();
    for f in rest {
        let audio = audio::decode_file(f, std::env::var_os("VOICETYPE_RAW").is_none())?;
        let t = Instant::now();
        let text = engine.transcribe(profile, &audio, false)?;
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
        _ => client(&args),
    }
}
