# VoiceType

Voice typing for Linux that runs on your own computer. Press a shortcut, speak, pause, and your words are typed into whatever window you're using: a terminal, a browser, Claude, Codex, WhatsApp Web, anything.

It's like Wispr Flow, for Linux, with no account, no cloud and no subscription.

- **Works everywhere.** It types through the system keyboard, so any app that accepts typing works.
- **Fast and private.** It uses OpenAI's Whisper (large-v3-turbo) through [whisper.cpp](https://github.com/ggml-org/whisper.cpp) on your GPU (Vulkan) or CPU. Audio never leaves your machine.
- **English dictation.** VoiceType transcribes English only, so there is no language detection step to slow it down.
- **Hands-free stop.** Speak, then pause for 2 seconds and it types. No second key press needed. It starts transcribing as soon as you go quiet, so the text is usually ready when the pause ends.
- **Compact popup.** A small pill near the bottom of the screen shows an audio-reactive waveform while listening, a countdown line while you pause, and animated dots while processing. No status labels; it expands only for finished text or errors.
- **Clean audio.** A DC filter removes input offset before RNNoise removes background noise and hum, and speech detection drops clips with nothing said, so it doesn't type made-up words.
- **Your names, spelled right.** Add names and brands (Claude, Anthropic, your colleagues) to a words list and Whisper uses them as a spelling hint. Fix-up rules catch anything it still gets wrong.
- **Optional text cleanup.** A small local model can fix your grammar or turn your speech into a clear prompt for AI tools. It is off by default, and your original words stay in History.
- **Tray icon and history.** Pick the microphone and auto-stop time from the tray. Every dictation is saved locally, so you can search it, copy it, or type it again.
- **Light.** One Rust binary. The background service uses about 155 MB of RAM plus the model in GPU memory.

## Requirements

- A **Wayland** desktop with layer-shell support: KDE Plasma, Sway, Hyprland and others. GNOME doesn't support layer-shell, so the popup won't show there, but typing still works.
- **PipeWire** for audio.
- **ydotool** for typing, and permission to use `/dev/uinput`. The installer tells you how to set this up if it's missing.
- Rust (cargo). For GPU speed you also need `glslc` and the Vulkan headers.
- Optional, for text cleanup: [Ollama](https://ollama.com) and about 2.3 GB more GPU memory.
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
5. on KDE, sets **Meta+Space** for dictation.

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

**Tray menu:** start, stop or cancel dictation, microphone, auto-stop after pause (Off, 1, 1.5, 2 or 3 seconds), text cleanup (Off, Grammar or Prompt), your last 10 dictations (click to copy), *Type last again*, *Edit words and names…*, and *History…*.

**Commands:**

```bash
voicetype toggle                # start or stop dictation
voicetype stop                  # stop and type now
voicetype cancel                # stop and throw the clip away
voicetype status                # recording / busy / idle
voicetype autostop 2            # seconds of silence before it types (0 = off)
voicetype cleanup off|grammar|prompt   # optional Qwen text cleanup
voicetype clean prompt "some text"    # try cleanup on text, without dictating
voicetype history               # open the history window
voicetype file clip.ogg …       # transcribe audio files
```

In **History…**, edit a transcript before using Copy or Type. These edits are temporary; Refresh or closing the window restores the saved transcript.

The waveform follows raw microphone levels and stays still when there is no signal. Detailed level diagnostics are available in service logs with `VOICETYPE_DEBUG=1`; they are hidden from the normal popup.

If the microphone disconnects or sends no audio for two seconds, recording stops. Any recovered transcript is saved to History for review instead of being typed automatically. PipeWire errors are available in the service log.

Settings are saved in `~/.config/voicetype/settings.json`. History is saved in `~/.local/share/voicetype/history.jsonl`.

## Names and words

Whisper doesn't know your colleagues' names or newer brand names, so "Claude Code" can come out as "Cloud Code". Open **Edit words and names…** from the tray (the file is `~/.config/voicetype/words.txt`) and add one name per line:

```
Claude Code
Anthropic
PlotDial
Manibarshit
```

The list goes to Whisper as a spelling hint with each dictation. Changes apply to your next dictation, with no restart needed.

If a word still comes out wrong, add a fix rule. It replaces whole words, ignoring upper and lower case:

```
cloud code => Claude Code
mani barshit => Manibarshit
```

## Text cleanup (optional)

VoiceType can pass each dictation through a small local model, Qwen 3.5 2B. Pick a mode under **Text cleanup** in the tray:

- **Off** (the default) types exactly what Whisper heard. Use it for terminal commands and exact wording.
- **Grammar** makes light fixes. Use it for messages to people.
- **Prompt** fixes grammar, removes restarts and rewrites your speech as clear sentences. Use it when talking to AI tools.

It needs [Ollama](https://ollama.com) running on this computer and about 2.3 GB more GPU memory:

```bash
ollama pull qwen3.5:2b
voicetype cleanup prompt
```

Text stays on your computer, and a typical clip takes about 0.3 s more on a laptop RTX 3050.

The model is small and sometimes changes what you meant. In a test on 57 real dictations, Grammar changed the meaning or invented details in about 1 clip in 11, and Prompt in about 1 clip in 7. Most Prompt mistakes were on clips that were already garbled or cut off. Grammar removed about half of the "I mean" corrections; Prompt removed nearly all of them. VoiceType never presses Enter, so read the text before you send it.

VoiceType types your original words instead if Ollama isn't running, takes longer than 5 seconds, or returns something suspicious: a reply far longer or shorter than what you said, a changed number of negative words, a request turned into a promise, your "I" turned into "you", or a made-up placeholder. Clips under 4 words are skipped. When cleanup changes a dictation, **History…** shows the original under it.

## Troubleshooting

- **It types nonsense like "So", "The" or "Thank you", or nothing.** Your microphone is probably too loud or picking up hum. Run `wpctl set-volume @DEFAULT_AUDIO_SOURCE@ 0.2`, or try another mic from the tray. The last clip is saved to `/run/user/$UID/voicetype-last.wav`, so you can listen to what it heard.
- **Nothing gets typed.** Check that `ydotool type hello` works in a terminal. If it doesn't, ydotoold isn't running or you can't write to `/dev/uinput`. Re-run `./install.sh` and follow its instructions.
- **Garbage output on a laptop with two GPUs.** VoiceType picks the discrete GPU automatically. To choose a GPU yourself, set `GGML_VK_VISIBLE_DEVICES=<n>` in the service. Device numbers are listed by `vulkaninfo --summary`.
- **Slow (several seconds per clip) on a laptop GPU.** If the laptop started on battery, the GPU can stay at battery speed even after you plug in. Run `nvidia-smi -q -d PERFORMANCE,POWER,CLOCK` while it transcribes: if the GPU sits in P5 with the memory clock far below its maximum, restart with the charger connected. On a Lenovo LOQ 15IRX9 with an RTX 3050, replugging the charger and locking the memory clock with `nvidia-smi` did not help, but a normal restart on the charger took memory from 810 to 7001 MHz and a 15-second clip from 2.5 s to 0.7 s. If it is still slow, or the GPU reports an impossible power draw (hundreds of watts), do a cold boot: shut down, unplug the charger, hold the power button for 15–20 seconds, plug the charger back in, then boot.
- **Logs:** `journalctl --user -u voicetype -f` shows each clip's length, how long it took, and the text.

## How it works

`voicetype daemon` runs as a user service. It holds the Whisper model in memory, draws the popup as a Wayland layer-shell surface (smithay-client-toolkit + tiny-skia + cosmic-text), shows the tray icon (StatusNotifierItem), and listens on a Unix socket for commands. Your shortcut runs `voicetype toggle`, which sends one line to that socket.

Audio comes from `pw-record` at 48 kHz and goes through RNNoise (`nnnoiseless`). It is resampled to 16 kHz and trimmed using RNNoise's speech probability, then sent to whisper.cpp. The popup shows a waveform while you speak, without decoding provisional previews. When you go quiet, it transcribes the whole clip at full quality, and if you don't start talking again, that result is the one it types. ASCII text is typed with `ydotool type`. Text with non-ASCII characters, such as accented names, is pasted through the clipboard, and your previous clipboard is restored afterwards.

## License

MIT, see [LICENSE](LICENSE). The Whisper model is also MIT.
