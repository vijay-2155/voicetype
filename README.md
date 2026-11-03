# VoiceType

Voice typing for Linux that runs on your own computer. Press a shortcut, speak, pause, and your words are typed into whatever window you're using: a terminal, a browser, Claude, Codex, WhatsApp Web, anything.

It's like Wispr Flow, for Linux, with no account, no cloud and no subscription.

- **Works everywhere.** It types through the system keyboard, so any app that accepts typing works.
- **Fast and private.** It uses OpenAI's Whisper (large-v3-turbo) through [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on your GPU (Vulkan) or CPU. Audio never leaves your machine.
- **Many languages.** Whisper detects about 100 languages. There's also an optional Telugu mode with a dedicated Telugu model.
- **Hands-free stop.** Speak, then pause for 2 seconds and it types. No second key press needed.
- **Live popup.** A small pill near the bottom of the screen shows your words as you speak, a level meter, and a countdown bar while you pause.
- **Clean audio.** RNNoise removes background noise and hum, and speech detection drops clips with nothing said, so it doesn't type made-up words.
- **Tray icon and history.** Pick the language, microphone and auto-stop time from the tray. Every dictation is saved locally, so you can search it, copy it, or type it again.
- **Light.** One Rust binary. The background service uses about 155 MB of RAM plus the model in GPU memory.

## Requirements

- A **Wayland** desktop with layer-shell support: KDE Plasma, Sway, Hyprland and others. GNOME doesn't support layer-shell, so the popup won't show there, but typing still works.
- **PipeWire** for audio.
- **ydotool** for typing, and permission to use `/dev/uinput`. The installer tells you how to set this up if it's missing.
- Rust (cargo). For GPU speed you also need `glslc` and the Vulkan headers.
- About 600 MB of disk for the speech model, plus about 1 GB of GPU memory, or it runs on the CPU (slower).

## Install

```bash
git clone https://github.com/Vijay-2155/voicetype.git
cd voicetype
./install.sh
```

The installer:

1. builds the app;
2. installs it to `~/.local/bin/voicetype`;
3. downloads the Whisper model to `~/.local/share/voicetype/models`;
4. sets up the background service (systemd user service);
5. on KDE, sets the shortcuts **Meta+Space** (dictate) and **Meta+Shift+Space** (Telugu).

On other desktops, bind a shortcut yourself to:

```
~/.local/bin/voicetype toggle
```

Options:

- `./install.sh --cpu` builds without the GPU backend.
- `./install.sh --no-model` skips the model download.

To update, run `git pull && ./install.sh` again.

To uninstall, run `./uninstall.sh`. Add `--purge` to also delete the models, history and settings.

## Use

1. Click where you want the text.
2. Press **Meta+Space** and speak.
3. Pause for 2 seconds. It transcribes and types. You can press Meta+Space again to stop early.

**Tray menu:** start or stop dictation, language (Auto / English / Telugu), microphone, auto-stop after pause (Off, 1, 1.5, 2 or 3 seconds), your last 10 dictations (click to copy), *Type last again*, and *History…*.

**Commands:**

```bash
voicetype toggle [auto|en|te]   # start or stop dictation
voicetype stop                  # stop and type now
voicetype cancel                # stop and throw the clip away
voicetype status                # recording / busy / idle
voicetype mode auto|en|te       # default language for Meta+Space
voicetype autostop 2            # seconds of silence before it types (0 = off)
voicetype history               # open the history window
voicetype file [auto|en|te] clip.ogg …   # transcribe audio files
```

Settings are saved in `~/.config/voicetype/settings.json`. History is saved in `~/.local/share/voicetype/history.jsonl`.

## Telugu mode (optional)

Auto-detect often mistakes Telugu for English, so there's a separate mode that uses [vasista22/whisper-telugu-medium](https://huggingface.co/vasista22/whisper-telugu-medium). To set it up, convert that model once. This needs Python 3, git and about 6 GB of temporary space:

```bash
./scripts/convert-telugu.sh
```

Then use **Meta+Shift+Space**, or pick Telugu in the tray. The model is larger, so Telugu is noticeably slower than English for now.

## Troubleshooting

- **It types nonsense like "So", "The" or "Thank you", or nothing.** Your microphone is probably too loud or picking up hum. Run `wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 0.2`, or try another mic from the tray. The last clip is saved to `/run/user/$UID/voicetype-last.wav`, so you can listen to what it heard.
- **Nothing gets typed.** Check that `ydotool type hello` works in a terminal. If it doesn't, ydotoold isn't running or you can't write to `/dev/uinput`. Re-run `./install.sh` and follow its instructions.
- **Garbage output on a laptop with two GPUs.** VoiceType picks the discrete GPU automatically. To choose a GPU yourself, set `GGML_VK_VISIBLE_DEVICES=<n>` in the service. Device numbers are listed by `vulkaninfo --summary`.
- **Logs:** `journalctl --user -u voicetype -f` shows each clip's length, how long it took, and the text.

## How it works

`voicetype daemon` runs as a user service. It holds the Whisper model in memory, draws the popup as a Wayland layer-shell surface (smithay-client-toolkit + tiny-skia + cosmic-text), shows the tray icon (StatusNotifierItem), and listens on a Unix socket for commands. Your shortcut runs `voicetype toggle`, which sends one line to that socket.

Audio comes from `pw-record` at 48 kHz and goes through RNNoise (`nnnoiseless`). It is resampled to 16 kHz and trimmed using RNNoise's speech probability, then sent to whisper.cpp. Short live previews use greedy decoding while you speak. ASCII text is typed with `ydotool type`. Other scripts, such as Telugu, are pasted through the clipboard, and your previous clipboard is restored afterwards.

## License

MIT, see [LICENSE](LICENSE). The models have their own licenses: Whisper is MIT, and vasista22/whisper-telugu-medium is Apache-2.0.
