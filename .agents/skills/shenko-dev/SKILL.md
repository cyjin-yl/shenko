---
name: shenko-dev
description: "Required Shenko development entry: product judgment, task-scoped guidance and optional methods."
---

# Shenko development philosophy

Why: build a personal agent with continuous experiences, relationships, commitments and interests, and replaceable implementations. Preserve all S1–S5 and latest explicit user decisions.

## Current reading policy (T6)

Read this and [AGENTS.md](../../../AGENTS.md) at current versions, then task-relevant links. [REFERENCE.md](REFERENCE.md) is byte-preserved current expanded guidance. Its full-read wording records prior loading policy; entries govern reading, and all substantive obligations remain effective.

## Product rules to keep active

- **Continuous thought:** one serial main loop continues after each finite generation, fairly yielding inference capacity to children. Semantic waiting still polls with the subject's own model and prior plus new context. Each wait has a semantic exit and **600-second maximum**, never reset by restart/migration/retry or silently renewed. Children may sleep; infrastructure outages are separate facts.
- **Mechanism and judgment:** nine kernel invariants cover durable facts, structural checks, wire, scheduling, storage and authorization; meaning belongs to the model. Plugins use **one lifecycle**, registering only in the **capability directory and hook graph**. Subject/ledger never name concrete plugins. No keyword dispatch, wording filters or extra extension paths.
- **Independent learning:** perception supplies sourced/versioned observations; awareness supplies perspectives and reasons; Skills supply methods; code obtains observations/effects. These remain independently discoverable, installable and replaceable. Any experience can yield awareness. Preserve sources, applicability and authority boundaries. Units express principles/observations, never scripts or expected answers. Dreaming is optional.
- **Behavioral evidence:** model-facing changes need real Store, subject step, provider wire and parser. Repeat several scenarios, record success rates and verbatim failures, and compare with the unit **disabled**. Judge product outcomes with a rubric on held-out scenarios, separate from writing examples; never weaken checks or use keyword filters. Local Qwen 3.8 27B NVFP4 intelligence is the reference baseline; model/endpoint come from the environment. Self-written units stay tentative until self-tested and effective beyond the disabled baseline; changed text/model requires retesting.
- **One self, real capabilities:** devices share self/memory/commitments; providers, credentials and access remain local facts. Verify an earlier identity before adoption. Connected operation has one primary; partitions elect the smartest actually usable LLM-accessible node, preserve experiences and reconcile effects. Use authorized SSH/device capabilities and actual privacy/access limits, without model-name gates or new human gates. Logging, credential-authorized export and disk recovery remain independent of LLMs.

## Follow the relevant detail

- [Thought](REFERENCE.md#保持一条真正持续的-thought-loop), [learning](REFERENCE.md#从经历中长出新的-awareness), [device continuity](REFERENCE.md#把多台设备作为同一自我的行动环境).
- Kernel/plugin changes: [kernel contract](../../../docs/specs/kernel.md).
- Model-facing changes: [behavior acceptance](../../../docs/development/local-model-behavior.md), [awareness efficacy](../../../docs/development/awareness-first-plugins.md).
- [Domain contracts and Why](../../../docs/INDEX.md); `SPEC.md`/`WHY.md` are navigation. Preserve public paths, raw data, immutable facts, receipts and Unknown outcomes.

## Choose a method when useful

Choose [optional companion methods](REFERENCE.md#按需要组合开发原子) for the actual task, with no required order, count or report. Reuse evidence and scoped checks; avoid a test factory.
