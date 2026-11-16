#!/usr/bin/env bash
set -euo pipefail
cd /workspace/shenko-obelisk-qwen4b-20261116
mkdir -p metadata downloads logs requests server
printf 'started_at=%s\n' "$(date -u +%FT%TZ)"
curl -fL --retry 2 --retry-delay 2 --connect-timeout 15 --max-time 1800 --continue-at - 'https://huggingface.co/Qwen/Qwen3-4B-GGUF/resolve/bc640142c66e1fdd12af0bd68f40445458f3869b/Qwen3-4B-Q4_K_M.gguf' -o downloads/Qwen3-4B-Q4_K_M.gguf
printf '%s\n' '7485fe6f11af29433bc51cab58009521f205840f5b4ae3a32fa7f92e8534fdf5  downloads/Qwen3-4B-Q4_K_M.gguf' > metadata/model.sha256
sha256sum -c metadata/model.sha256
printf 'finished_at=%s\n' "$(date -u +%FT%TZ)"
touch metadata/model-download.ok
