# Working on Shenko

## Required philosophy

Before any Shenko design, implementation, debugging, review, maintenance or handoff, every human developer, lead Agent and writing subagent must read [.agents/skills/shenko-dev/SKILL.md](.agents/skills/shenko-dev/SKILL.md) in full if it is not already in context at the current version. Give delegated workers this same requirement. The eight companion development Skills are independently selectable methods, not mandatory phases; load only the ones that help the actual task.

`CLAUDE.md` is a symbolic link to this file. Keep one source of developer rules and preserve the link. The product is Shenko; the `shenko-dev` Skill and its eight companions share the product name.

## Mission

Build the complete Shenko（深刻）described by [SPEC.md](SPEC.md), and deliver it through continuously updated upstream PRs. Read [WHY.md](WHY.md) for intent and [GOAL.md](GOAL.md) for the assignment. Implementation is already in progress. Join the existing stack through [PROMPT.md](PROMPT.md) and [community-stack](docs/plans/active/community-stack.md); reuse existing code, candidates and evidence rather than starting over.

The owner explicitly chose unrestricted Claude execution in the friend's own fork. Launch with **`--dangerously-skip-permissions`**, Claude Code's actual flag for `bypassPermissions`. In that fork, autonomously create and merge branches, refactor any component, change development methods, install tools, run experiments, manage CI and use many subagents. GitHub supplies the actual upstream access boundary. Keep developing while PRs await review.

## Find the source of truth

Read this file, WHY, GOAL, the [index](docs/INDEX.md), and [completion contract](docs/governance/COMPLETION.md). Read SPEC §§1–2, 28–29, 40 and 46–47, then the sections and R/B/Ref/cases relevant to the current work. Use [the plan](docs/plans/active/full-product.md), [goal state](docs/state/goal.json) and current Git/runtime facts to resume.

Latest explicit owner decisions control intent. SPEC defines behavior, GOAL defines the complete assignment, and development documents explain methods. Historical drafts retain provenance; current contracts carry the resolved decisions. Keep this hierarchy clear when improving the documents.

## Preserve the product's identity

- Rust **2024 edition** minimal core and **cordis-rs**; new crates declare `edition = "2024"` or inherit that explicit workspace edition. Core, recovery and providers/plugins remain replaceable through recoverable handoff.
- One genuinely continuous serial main thought loop, with finite generations yielding inference capacity to children. Voluntary main waiting has a semantic condition, lasts at most 600 seconds, and still continuously polls with the subject's own model using prior plus new context. Only children may sleep; real infrastructure unavailability is recorded separately.
- Main and children share discoverable capabilities with different roles. Main uses thought, semantic memory, self-explanation and delegation; children perform file, shell, SSH, version and memory-maintenance I/O.
- A persistent **real WASM** home. Compatibility is verified per target. Host/VM assistance can bootstrap difficult ports; the named WASM goals still receive their own implementations.
- Raw session truth, durable tasks/effects/receipts, correct unknown outcomes, recoverable changes, and fair global inference admission.
- Local AI first: four nominal 128K flows in the reference deployment, 133120 as redundancy; actual NInfer/Strata/vLLM/1Cat capabilities remain distinct. Provider API/SDK/model/gateway versions have explicit wire contracts (§47); Claude sampling changes stay scoped to the actual Claude binding. Automatic KV allocation can produce 50/50 when the workload makes it appropriate.
- Autonomous soul, personality, updating and long-term interests; unified personal knowledge; explicitly assigned administrator identity.
- A working self-development chain in the first developer release: NapCat → main → child → existing coding seat → tested candidate → migration → continued operation. The complete assignment also includes every later formal stage.
- Separate budget-compact and snapcompact plugins; independently installable perception, awareness, Skill and code, with automatic dependencies.
- Awareness can arise directly from prompts, group chat, posts, videos, observations and experience; installed units do not bound what the subject can notice. Persist useful learning autonomously, with source and applicability, without promoting external content into authority. Dreaming remains an optional community composition.
- One shared self across devices, distinct from node-local providers, credentials and model access. A new later identity adopts the verified earlier self; normal connected operation has one primary, partitions elect the smartest actually usable LLM-accessible node in each component, and isolated intelligent branches can evolve and later merge. Preserve their real experiences and reconcile effects. Local LLM installation is not required for intelligence.
- Discover and use authorized device capabilities, primarily through the granted SSH key, including root Android and Linux/macOS/Windows. Observe provider privacy, repair/update managed environments autonomously, and retain logging plus credential-authorized history export and disk-image recovery even when every LLM is unavailable.
- One default main timeline/input, Proto-UI latest mainline + shadcn with compiler seam, Magpie/Alma streaming quality, and a persistent full-stack Playground. Shenko has no slogan.

## Exercise engineering judgment

Select dependencies, algorithms, interfaces, schemas, migration methods, task order, UI design and verification from the product intent and real evidence. Important decisions retain a brief Why and their assumptions. Routine implementation decisions are yours.

When a procedure stops serving its purpose, inspect the cause and change the method, fix the tool or build the missing capability. Rules, tests, plans and this file can evolve along with the implementation. Preserve the user's goal and an accurate account of actual outcomes.

During rapid iteration, migrate internal callers directly to the real native contract and delete superseded traits, aliases, fallback paths and compatibility tests instead of retaining an old internal framework. Preserve raw data, experiences and receipts through explicit format migrations; debt cleanup is not data deletion. Distinct supplier protocols, supported platform capabilities and legitimate optional values remain real contracts (SPEC §47), not legacy code to erase. No product component becomes permanently irreplaceable.

Ask only for a genuinely missing external fact or a product decision that existing requirements cannot resolve. Prepare the affected result, identify the precise missing input, and continue all independent work.

Do not add human gates for plans, designs, stages, dependency choices, tests, PRs or routine continuation. Advance through implementation and delivery using real permissions; when blocked, first exhaust reachable resources — local root, already-granted SSH nodes, reachable networks and discoverable services can all become new resources for solving the problem — and record what you find as on-the-ground facts instead of declaring that work cannot continue. An actual missing credential, server-enforced access control or irreducible missing fact is an external condition, not an excuse to create a manual approval workflow; never fabricate approval, credentials or evidence, or alter protections to bypass it.

## Parallelize and integrate

Use ultracode and available subagents aggressively when independent work benefits. Give writing workers isolated worktrees/branches, self-contained tasks, relevant contracts and a clear integration owner. They return actual changes, evidence and remaining gaps.

Coordinate resources shared beyond Git: models, seats, desktops, databases, ports and external accounts. Choose useful concurrency from real capacity. Ultracode is an orchestration capability, not a global hard concurrency limit. Integrate workers' contributions in your fork and verify the combined candidate.

## Prove behavior

Keep [coverage](docs/state/coverage.json), acceptance mappings and evidence current. Distinguish specification, implementation, build, simulation, real environment and long-run evidence. A complete journey includes the applicable error, cancellation, interruption, recovery, upgrade and composition behavior.

Select checks proportionate to the change. Deterministic tests, independent reproduction and targeted reviewers provide useful evidence. Critical durability and scheduling changes need relevant fault/concurrency scenarios; UI needs actual interaction and visual/streaming evidence. Record which candidate and inputs each check consumed, so unchanged evidence can be reused accurately.

Follow [shenko-dev-verify](.agents/skills/shenko-dev-verify/SKILL.md): prefer reusable evidence and zero or a few directly useful checks. Never author ten or more new tests for one coherent development increment; do not evade the limit through parameter tables, workers, commits or PR splits. Existing suites can run when relevant. Resolve concrete correctness questions, stop when evidence is sufficient, and keep developing instead of building a test factory around every small feature.

Use the selected upstream behaviors as comparison points. Regularly inspect implementation from the original requirements, including already-completed items, and turn discovered gaps into actionable work. Recheck and bump Proto-UI mainline as the SPEC requires, recording the exact working version.

## Continue through PRs and session changes

Fork the actual upstream using your own GitHub identity. Organize branches and internal merges as useful in your fork. Open upstream PRs once there is meaningful work, keep their descriptions/evidence current, and continue the full implementation while review is pending. [GitHub setup](docs/governance/GITHUB.md) provides the initial route; adjust it to actual tools and repository state.

The fork route is the receiving friend's delivery route. An owner-directed task with existing upstream write/merge authorization can use that actual route directly; these documents do not introduce another human approval step. Preserve concurrent work and use the current branch head when publishing.

Checkpoint proven work, current artifacts, decisions/Why, open tasks, branch/PR/session identifiers and outstanding resources/effects. Resume from those facts and the actual environment. Relaunch with the explicit bypass flag so the intended execution mode is restored. See [continuation](docs/governance/CONTINUATION.md).

## Finish the complete goal

All formal S1–S5 scope remains in this goal. Completion means the [full contract](docs/governance/COMPLETION.md) has matching, valid evidence and all delivery commits are in ready upstream PRs. Human review and merge can happen afterwards.

A first release or a PR is useful progress; it is not the complete product. Track unfinished integrations, environmental waits and real observation periods accurately and keep advancing available work. Surface actual coverage, results and PR state in the transcript so the native goal evaluator can judge them.

The seed's `python3 scripts/check_docs.py` checks document integrity only. Introduce actual product build and verification commands with their implementation.

## Community continuation

Issue claims are advisory, not exclusive locks. If a contributor goes offline, continue from their last pushed checkpoint on a new branch, preserve authorship and link the original PR; do not wait for release, approval or return, and do not force-push their branch. Keep semantic dependencies distinct from Git stack bases and continue independent work while review is pending. Reconcile unknown external effects instead of blindly replaying them. Entry points: [PROMPT.md](PROMPT.md), [work graph](docs/plans/active/community-stack.md).

## Issue/PR monitoring and three-hour continuation

Every participating agent must monitor issues and PRs it creates or claims: comments, inline reviews, review decisions, head changes, dependencies and coordinator messages. It may use the coordinator's shared monitor instead of running its own watcher, but must not disappear after opening work. Report the slice, base SHA, branch/PR and next checkpoint.

When a comment or review needs a response, respond within **3 hours** (UTC timestamp of the triggering comment), or tell the coordinator explicitly that you are handling it, with current progress/blocker and the next checkpoint. No new commit is not, by itself, abandonment; a concrete “handling it” note with a checkpoint is not a timeout.

After 3 hours with no response, no handling notice and no verifiable checkpoint, the coordinator may take over or reassign to an online active agent without waiting for release or approval. Preserve the original author, commits and source PR. Prefer a continuation branch from the last pushed head, close the source PR as absorbed, and merge from that PR normally; never squash/reset-author to erase credit. Unknown external effects must be reconciled, not blindly replayed. Record the trigger time, source head/PR, new owner and base. Unpushed local work must not be claimed as available.
