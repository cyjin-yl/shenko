# Real single-node journey

`scripts/real_single_node_journey.py` is an explicit bounded observation of a
real Shenko CLI and a real configured OpenAI-compatible model. It is not run
as a substitute for the normal Rust checks or the completion contract.

The script starts a fresh installation, submits a user message through `/ws`,
and asks the actual subject to delegate a finite model child that computes
19 × 23 and a native `host.printf` child that executes a host process. It reads
the task, operation, attempt, E1, result, and committed decision ledgers without
writing them. A matching piece of model text alone cannot pass: both real child
results and their evidence must exist, and the reported output must commit
after those results.

It then stops the owned process, starts the same installation, resubmits the
same input identity, and asks the model to continue the preceding task from its
existing context. The result checks the original input event/commit identity,
subject and branch identity, retained task results, and a newly committed answer.
No model output, successful task, or receipt is supplied by the driver.

## Run

Use Python with `websockets==16.0`, an actual model endpoint, and a CLI built from
the revision named in the command. Preserve the model checkpoint/quantization,
runtime identity, context size, and sampling parameters with the run's evidence.

```sh
SHENKO_SOURCE_REVISION=$(git rev-parse HEAD) cargo build --locked -p shenko -j2
python scripts/real_single_node_journey.py \
  --binary target/debug/shenko \
  --source-revision "$(git rev-parse HEAD)" \
  --model-url http://127.0.0.1:18832/v1 \
  --model Qwen3-0.6B-Q4_K_M \
  --timeout 300 --evidence-dir /tmp/shenko-real-single-node
```

The evidence directory must not already exist. Each phase has a bounded
observation interval. A failure exits nonzero and keeps its original runtime
logs, input, WebSocket frames, read-only ledger snapshots and CAS. The
`installation` subdirectory contains private bootstrap/control material and is
excluded from the hosted artifact; publish only the explicit evidence subset.

The manual `Real single-node journey` GitHub Actions workflow builds the exact
selected repository revision, verifies pinned model and llama.cpp archive
SHA-256 values, and runs the same script. It retains failed observations too.
The fixed 0.6B model is a low-resource mechanical experiment. Its outcomes do
not satisfy the Qwen3.8-27B quality baseline, the NapCat/coding-seat S2 journey,
physical fault requirements, or 7/30-day observation periods.

## 2026-11-16 local attempt

The available pre-existing CLI was identified by SHA-256
`0a17e5e97dd97de231007d5a6f4e37285a080d31ff74e96e82f0b9419d34a549`,
with **unverified source provenance**. No runtime acceptance was attributed to
that binary or to the current policy-config implementation.

Two real model loading attempts were killed with exit 137 before the endpoint
became ready: official Qwen3-0.6B Q8_0 and the smaller Unsloth Q4_K_M with ordinary
file loading. An intervening option preflight failed because llama.cpp b12359
uses `--load-mode none` rather than the older `--no-mmap`. Both actual loading
failures, the option failure, artifact identities and resource observations are
retained in [the raw record](../evidence/single-node-model-load-20261116/outcome.json).
Resource exhaustion is suspected; no kernel attribution was available. The
model received **zero requests**, and the single-node journey was not launched.

The local Rust verification command also stopped before tests ran: `rustc`
compiling `shenko-store` was killed with SIGKILL. The new policy tests therefore
require execution against the current candidate in an environment with enough
resources. These failures remain open evidence, not successful acceptance.
