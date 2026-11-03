use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

pub fn type_text(text: &str) {
    if text.is_empty() {
        return;
    }
    if text.is_ascii() {
        let _ = Command::new("ydotool").args(["type", "-d", "2", "-H", "2", "--", text]).status();
        return;
    }
    let old = Command::new("wl-paste").arg("-n").output().ok().map(|o| o.stdout);
    let _ = Command::new("wl-copy").args(["--", text]).status();
    std::thread::sleep(Duration::from_millis(80));
    let _ = Command::new("ydotool").args(["key", "29:1", "42:1", "47:1", "47:0", "42:0", "29:0"]).status();
    std::thread::sleep(Duration::from_millis(400));
    if let Some(old) = old.filter(|o| !o.is_empty()) {
        if let Ok(mut c) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
            if let Some(mut i) = c.stdin.take() {
                let _ = i.write_all(&old);
            }
            let _ = c.wait();
        }
    }
}
