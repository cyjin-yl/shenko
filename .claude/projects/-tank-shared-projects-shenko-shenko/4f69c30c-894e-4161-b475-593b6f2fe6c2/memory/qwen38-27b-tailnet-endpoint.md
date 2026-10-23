---
name: qwen38-27b-tailnet-endpoint
description: Qwen3.8-27B vLLM endpoint on Tailnet for SPEC §22.1 baseline testing
metadata:
  type: reference
---

Qwen3.8-27B vLLM endpoint (SPEC §22.1 首个真实评测基线):
- Base URL: https://doesvm.icefish-exponential.ts.net:8446
- Anthropic-compatible: https://doesvm.icefish-exponential.ts.net:8446/v1/messages (anthropic-version: 2026-10-21, x-api-key: dummy)
- OpenAI-compatible: https://doesvm.icefish-exponential.ts.net:8446/v1/chat/completions
- Models: qwen3.8-27b, qwen3.8-27b-anth, swift-qwen3.8-27b-nvfp4, auto
- Max context: 262144 tokens
- Real model: Merkyor-Coder390-NVFP4-W4A16
- No auth needed (Tailnet only)
- Verified 2026-10-23: returns thinking + text blocks, stop_reason end_turn
