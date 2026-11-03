#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"
export PATH="$HOME/.cargo/bin:$PATH"

FEATURES="--features vulkan"
GET_MODEL=1
for arg in "$@"; do
    case "$arg" in
        --cpu) FEATURES="" ;;
        --no-model) GET_MODEL=0 ;;
        -h|--help)
            echo "usage: ./install.sh [--cpu] [--no-model]"
            echo "  --cpu       build without the Vulkan GPU backend"
            echo "  --no-model  skip downloading the speech model (574 MB)"
            exit 0 ;;
        *) echo "unknown option: $arg" >&2; exit 1 ;;
    esac
done

BIN_DIR="$HOME/.local/bin"
BIN="$BIN_DIR/voicetype"
DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
MODELS="${VOICETYPE_MODELS:-$DATA/voicetype/models}"
APPS="$DATA/applications"
UNITS="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"
MODEL_URL="https://huggingface.co/ggerganov/whisper.cpp/resolve/main/ggml-large-v3-turbo-q5_0.bin"

say() { printf '\033[1;35m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m!!\033[0m %s\n' "$*"; }

missing=()
for cmd in cargo pw-record pactl wl-copy wl-paste ydotool ydotoold curl; do
    command -v "$cmd" >/dev/null || missing+=("$cmd")
done
if ((${#missing[@]})); then
    warn "missing: ${missing[*]}"
    echo "Install them first, for example:"
    echo "  Fedora:        sudo dnf install cargo pipewire-utils pulseaudio-utils wl-clipboard ydotool curl glslc vulkan-loader-devel"
    echo "  Ubuntu/Debian: sudo apt install cargo pipewire-bin pulseaudio-utils wl-clipboard ydotool curl glslc libvulkan-dev cmake clang"
    echo "  Arch:          sudo pacman -S rust pipewire libpulse wl-clipboard ydotool curl shaderc vulkan-icd-loader cmake clang"
    exit 1
fi

if [[ -n "$FEATURES" ]] && ! command -v glslc >/dev/null; then
    warn "glslc not found, building for CPU only (install glslc/shaderc for GPU speed)"
    FEATURES=""
fi

say "Building (this takes a few minutes the first time)"
cargo build --release $FEATURES
install -Dm755 target/release/voicetype "$BIN"
say "Installed $BIN"

if ((GET_MODEL)); then
    mkdir -p "$MODELS"
    if [[ ! -s "$MODELS/ggml-large-v3-turbo-q5_0.bin" ]]; then
        say "Downloading Whisper large-v3-turbo (574 MB) to $MODELS"
        curl -L --fail -C - -o "$MODELS/ggml-large-v3-turbo-q5_0.bin.part" "$MODEL_URL"
        mv "$MODELS/ggml-large-v3-turbo-q5_0.bin.part" "$MODELS/ggml-large-v3-turbo-q5_0.bin"
    else
        say "Model already present"
    fi
fi

mkdir -p "$APPS" "$UNITS"
for f in packaging/*.desktop; do
    sed "s|@BIN@|$BIN|" "$f" > "$APPS/$(basename "$f")"
done
install -m644 packaging/voicetype.service "$UNITS/voicetype.service"

if ! pgrep -x ydotoold >/dev/null && ! systemctl is-active --quiet ydotool 2>/dev/null; then
    install -m644 packaging/ydotool.service "$UNITS/ydotool.service"
    YDOTOOL_USER=1
fi

if [[ ! -w /dev/uinput ]]; then
    warn "Your user cannot write /dev/uinput, so typing will not work yet. Run once, then log out and back in:"
    echo "  echo 'KERNEL==\"uinput\", MODE=\"0660\", GROUP=\"input\"' | sudo tee /etc/udev/rules.d/99-uinput.rules"
    echo "  sudo udevadm control --reload && sudo udevadm trigger"
    echo "  sudo usermod -aG input \$USER"
fi

systemctl --user daemon-reload
if [[ "${YDOTOOL_USER:-0}" == 1 ]]; then
    systemctl --user enable --now ydotool.service || warn "could not start ydotoold"
fi
systemctl --user enable voicetype.service >/dev/null
systemctl --user restart voicetype.service

if [[ "${XDG_CURRENT_DESKTOP:-}" == *KDE* ]] && command -v kwriteconfig6 >/dev/null; then
    kwriteconfig6 --file kglobalshortcutsrc --group services --group voicetype-auto.desktop --key _launch "Meta+Space"
    kwriteconfig6 --file kglobalshortcutsrc --group services --group voicetype-te.desktop --key _launch "Meta+Shift+Space"
    command -v kbuildsycoca6 >/dev/null && kbuildsycoca6 >/dev/null 2>&1 || true
    say "Shortcuts set: Meta+Space (dictate), Meta+Shift+Space (Telugu). Log out and in if they don't respond yet."
else
    say "Bind a keyboard shortcut in your desktop settings to:  $BIN toggle"
fi

say "Done. Press your shortcut, speak, and pause for 2 seconds."
