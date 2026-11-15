#!/usr/bin/env bash
set -euo pipefail
cd /workspace/shenko-obelisk-qwen4b-20261116
test -f metadata/model-download.ok
exec env -i PATH=/usr/bin:/bin OMP_NUM_THREADS=4 \
  /workspace/shenko-obelisk-qwen4b-20261116/server/llama-b12359/llama-server \
  --model /workspace/shenko-obelisk-qwen4b-20261116/downloads/Qwen3-4B-Q4_K_M.gguf \
  --alias Qwen3-4B-Q4_K_M --host 127.0.0.1 --port 18834 --offline --no-webui \
  --threads 4 --threads-batch 4 --threads-http 2 \
  --ctx-size 16384 --parallel 1 --batch-size 512 --ubatch-size 128 \
  --n-gpu-layers 0 --flash-attn on --cache-type-k q8_0 --cache-type-v q8_0 \
  --cache-ram 64 --no-repack \
  --jinja --chat-template-kwargs '{"enable_thinking":false}' --reasoning off \
  --n-predict 1024 --no-context-shift \
  --temp 0.7 --top-p 0.8 --top-k 20 --min-p 0 --presence-penalty 1.5 \
  --timeout 600 --log-timestamps --log-colors off --verbosity 4
