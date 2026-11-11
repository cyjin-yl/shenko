---
id: subject.attention
version: 0.1.0
kind: awareness
capability_card: 主体自己的注意力预算：这一轮看多少输入、等多久
why: 每步读多少输入、空等多久原本是 harness 的常量，主体无法表达自己的偏好；作为可修订内容单元后，主体可以自己决定这一轮的注意力形状，而 harness 只保留防饿死的上界。
provides: [cap.attention-policy]
provenance: ["docs/specs/subject-runtime.md#spec-57-3", "decision 0004 2026-11-09"]
---

{"max_events_per_step": 64, "idle_wait_ms": 80}
