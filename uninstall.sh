#!/usr/bin/env bash
set -euo pipefail

DATA="${XDG_DATA_HOME:-$HOME/.local/share}"
UNITS="${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user"

systemctl --user disable --now voicetype.service 2>/dev/null || true
rm -f "$HOME/.local/bin/voicetype" "$UNITS/voicetype.service" \
    "$DATA/applications/voicetype-auto.desktop" "$DATA/applications/voicetype-te.desktop" "$DATA/applications/voicetype-history.desktop"
systemctl --user daemon-reload

if [[ "${1:-}" == "--purge" ]]; then
    rm -rf "$DATA/voicetype" "${XDG_CONFIG_HOME:-$HOME/.config}/voicetype"
    echo "Removed VoiceType, its models, history and settings."
else
    echo "Removed VoiceType. Models, history and settings are kept in $DATA/voicetype (use --purge to delete them)."
fi
