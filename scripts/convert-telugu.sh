#!/usr/bin/env bash
set -euo pipefail

MODELS="${VOICETYPE_MODELS:-${XDG_DATA_HOME:-$HOME/.local/share}/voicetype/models}"
WORK="${1:-$(mktemp -d)}"
mkdir -p "$WORK" "$MODELS"
cd "$WORK"

echo "Working in $WORK (needs about 6 GB free; delete it afterwards)"
[[ -d whisper.cpp ]] || git clone --depth 1 https://github.com/ggml-org/whisper.cpp
[[ -d whisper ]] || git clone --depth 1 https://github.com/openai/whisper
[[ -d venv ]] || python3 -m venv venv
. venv/bin/activate
pip install -q --index-url https://download.pytorch.org/whl/cpu torch
pip install -q transformers numpy huggingface_hub

python - <<'EOF'
from huggingface_hub import snapshot_download
snapshot_download("vasista22/whisper-telugu-medium", local_dir="telugu")
EOF

mkdir -p out
python whisper.cpp/models/convert-h5-to-ggml.py telugu whisper out
mv out/ggml-model.bin "$MODELS/ggml-telugu-medium.bin"
echo "Saved $MODELS/ggml-telugu-medium.bin. Telugu mode (Meta+Shift+Space) is ready."
