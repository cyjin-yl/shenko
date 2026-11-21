# Nightly environment repair — 2026-11-22

This record follows the environment repair and its executed candidate receipts.
The initial repair changed no Rust assertion, compiler expectation, Proto-UI
source pin or selected test file. Later fixture corrections and pin adoption
are recorded separately below. Each successful run remains tied to its own
candidate; an environment repair alone is not a green Nightly receipt or
real-model daily-use acceptance.

## Observed baseline

Source: `f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`,
[Nightly 44896952438](https://github.com/cyjin-yl/shenko/actions/runs/44896952438).

| Job | Original observation | Scope |
| --- | --- | --- |
| [Rust long](https://github.com/cyjin-yl/shenko/actions/runs/44896952438/job/136824571376) | `dsh_tools_keep_native_receipts_across_restart_and_disable` could not execute Node. Two `shenko-memory` knowledge tests and `newer_unrelated_facts_do_not_crowd_out_scoped_recall` failed because `SHENKO_NATIVE_ARTIFACT_ROOT` was absent. | Four tests across three targets failed during fixture preparation. The job continued through later targets and retained the original failures. |
| [Compiler full](https://github.com/cyjin-yl/shenko/actions/runs/44896952438/job/136824571668) | Ten suites failed at Chromium launch: running as root without `--no-sandbox` is unsupported. The run reported 173 passing files, two skipped files, 2,816 passing tests and 106 skipped tests. | The ten browser suites did not execute their product assertions. Their failure is not evidence of ten distinct compiler defects. |

## Repair and Why

The normal Rust CI and Nightly now use
[`native-fixtures`](../../.github/actions/native-fixtures/action.yml), one shared
composite action. It installs the Node version from `.node-version`, installs
DSH's committed npm lock with lifecycle scripts disabled, and binds all native
fixture paths to the actual runtime `GITHUB_WORKSPACE/.ci/evidence` through
`GITHUB_ENV`. This also makes the same bindings available to Nightly's later
explicit partition/recovery step. The required native artifact and DSH
directories are created; paths whose fixture requires a fresh directory are
left absent. CI/Nightly no longer maintain separate copies of these prerequisites.

The complete compiler suite runs as the non-root `ubuntu-24.04` hosted runner
user with its installed `/usr/bin/google-chrome`. Node and pnpm remain pinned by
the existing files, and the fetched Proto-UI commit remains
`f29738eb6a804ccfc58510cc5a5bd1692690716e`. The real browser requires a host
environment with working process isolation: the pinned
[browser harness](https://github.com/Proto-UI/Proto-UI/blob/f29738eb6a804ccfc58510cc5a5bd1692690716e/apps/www/src/content/docs/zh-cn/browser-harness.ts)
sets `chromiumSandbox: true`, and the native control-label suite explicitly
requires that sandbox. The hosted image's browser is declared by
[runner-images](https://github.com/actions/runner-images/blob/main/images/ubuntu/Ubuntu2404-Readme.md),
and its installation uses
[`/usr/bin/google-chrome`](https://github.com/actions/runner-images/blob/main/images/ubuntu/scripts/build/install-google-chrome.sh).
Rust and the independently rebuilt web assets continue to use their existing
development containers. No Chromium flag, host security policy or Docker
seccomp profile is relaxed to make these tests pass.

Before the complete compiler suite,
[`compiler_browser.mjs`](../../scripts/ci/compiler_browser.mjs) uses the actual
upstream installation of `playwright-core` to launch the selected browser with
`chromiumSandbox: true`. It records source, Node, Playwright, browser and runner
image identities, the UID, and the real `chrome://sandbox` page. A root process,
missing browser, launch error or cleanup failure is fatal and retained in
`.ci/evidence/compiler-browser.json`; there is no sandbox-disable fallback.
The full suite still runs as:

```sh
pnpm exec vitest run packages/compiler --maxWorkers=1 --minWorkers=1
```

## Verification at this source checkpoint

- Parsed the composite action and both workflows as YAML.
- Executed the action's actual Python environment-binding body: 13 bindings
  resolved inside a retained workspace fixture, the required directories
  existed, Node resolved to a real executable, and the partition directory
  remained fresh and absent.
- Confirmed both Rust jobs select that same action and the complete compiler
  command is retained without a sandbox-disable flag.
- `node --check scripts/ci/compiler_browser.mjs` and `git diff --check` passed.
- `scripts/check_docs.py` passed with its declared parser dependencies installed
  (`documentation_integrity_only`, zero errors; no product execution claim).

These are configuration and preparation checks. The separate execution receipts
below retain their actual candidate identities; an environment repair does not
turn the original red run into a claimed pass.

## Executed candidate: `a716ff15`

[Nightly 44929495546](https://github.com/cyjin-yl/shenko/actions/runs/44929495546)
checks out `a716ff15bbfe3bea67109ae8811ad3f8b5f335a4`. The observations below are
from that source and the 2026-11-21 UTC execution window. They precede the later
integration commits and cannot certify the final integrated candidate; its
Nightly must retain a separate receipt at its own SHA.

### Complete compiler suite: passed

[Compiler job 136922788338](https://github.com/cyjin-yl/shenko/actions/runs/44929495546/job/136922788338)
completed successfully. The unchanged complete-suite command reported **183
passing files and two skipped files**, and **2,913 passing tests and nine
skipped tests**, in **794.59 seconds**. There were no failed suites or tests.

The real browser preflight also passed. Its retained receipt records UID 1001,
Node `v24.22.1`, Playwright `1.58.2`, Google Chrome `157.0.8103.78`, and hosted
image `ubuntu24` version `20261116.376.1`. It launched
`/usr/bin/google-chrome` with `chromiumSandbox: true`; the observed
`chrome://sandbox` page reports the namespace sandbox, PID namespaces, network
namespaces, and Seccomp-BPF including TSYNC enabled. The upstream source remains
`f29738eb6a804ccfc58510cc5a5bd1692690716e`; the receipt's `upstream` identity is
copied from the actual `.ci/web/inputs.json` field `pin`.

Each of the ten suites that previously failed during browser setup now reports
a pass. Their 97 executed tests account for the increase from 2,816 to 2,913
passing tests and the decrease from 106 to nine skipped tests:

| Suite under `packages/compiler/test/` | Passing tests |
| --- | ---: |
| `browser-button-ssr.test.ts` | 21 |
| `browser-differential.test.ts` | 3 |
| `browser-native-lifecycle.test.ts` | 5 |
| `browser-pointer-props.test.ts` | 1 |
| `browser-source-presentation.test.ts` | 1 |
| `browser-update-intent.test.ts` | 2 |
| `control-label-native.browser.test.ts` | 24 |
| `focus-scope-native.browser.test.ts` | 22 |
| `vue2-native-editor.browser.test.ts` | 6 |
| `web-component-native.browser.test.ts` | 12 |

The [compiler evidence artifact](https://github.com/cyjin-yl/shenko/actions/runs/44929495546/artifacts/14717137265)
retains the complete test output, browser receipt and source pin. Artifact ID
`14717137265`, zip SHA-256
`697efd02d2376bd8bf095ea1fd96f86ea1ecd5579f606ca4427c0ea70b8e5e32`.

### Rust long suite: fixture failure exposed after environment repair

[Rust job 136922788729](https://github.com/cyjin-yl/shenko/actions/runs/44929495546/job/136922788729)
completed with a failure in the full workspace command. All four tests that
failed in the baseline because of missing Node or native artifact paths now
report `ok`, including the DSH restart/disable journey and all three memory
cases. The shared fixture setup also passed.

The workspace command invoked 125 unit/integration targets and 33 doctest
targets. Taking the final libtest summary per target, excluding nested child
process summaries, gives **1,236 passed, one failed and 11 ignored** across 157
summaries. The remaining target, `cognition_self_learning`, explicitly skipped
because no reference model configuration or binary was supplied; this run is
not real-model acceptance. The later explicit partition/recovery,
native-helper lost-reply recovery and native-helper identity refusal proofs
all passed. Their separate results do not erase the workspace failure.

The sole failing test is the existing
`loop_runner::recovery_tests::semantic_wait_is_durable_and_never_renewed_by_polling`:

```text
Error: conflict: semantic wait origin has no executable provider binding
test result: FAILED. 149 passed; 1 failed; 7 ignored; 0 measured; 0 filtered out
```

This failure comes from test assembly, not a missing system dependency or a
resource timeout. The existing test created its first S0 with a bare
`SubjectLoopState` and injected a recorded model result, leaving
`provider_binding: null` in the wait's original context. The new semantic-wait
route correctly refuses to execute that non-binding on the next step. The
test fixture is repaired by installing the production local-provider broker
before the first S0, retaining the real sealed input as the wait origin, and
checking both the source context reference and its frozen provider binding.
The existing changed-binding refusal, deadline and continue/end/replace
assertions remain. No production binding check is relaxed and no test is
skipped or added. The targeted rerun and full final-candidate receipt remain
required; this documented failing run is not reclassified as a pass.

The [native evidence artifact](https://github.com/cyjin-yl/shenko/actions/runs/44929495546/artifacts/14717813447)
has ID `14717813447` and zip SHA-256
`20c418e88f26d1a46c538c8e44b724cfd419062feb73c4085374934b2b17acbf`.
Its actual `workspace-test.log` was independently extracted through bounded ZIP
ranges with entry CRC validation; the 246,573-byte log has SHA-256
`05cb61f2dc252ba04444e343bb94fdbb2c73b91ddc9b925a38ea774950b54fef`
and contains the same sole failure and four repaired-case results as the job
log. Only about 1.2 MB of the 1.1 GB artifact was read for this diagnosis.

### Contemporaneous regular CI: incomplete because of cancellation

[CI 44929494626](https://github.com/cyjin-yl/shenko/actions/runs/44929494626)
reported five successful jobs (`docs-migration`, `web`, `kernel-boundary`,
`clippy-ratchet`, and `docs`), while
[Rust job 136922781872](https://github.com/cyjin-yl/shenko/actions/runs/44929494626/job/136922781872)
was cancelled. The Rust log binds its source to the PR merge candidate
`bd14c6e8aecbf5c3bdb1b10981cb5002f6de441d`, distinct from the Nightly checkout.

The Rust job successfully installed the shared native fixtures and completed
the reported preceding test targets. At 17:33:39 UTC it received
`context canceled` during
`ssh_wasi_auth_exec_sftp_and_refusal_journey`, before that test returned a result.
The log does not establish the cause of the external cancellation. This is
neither a six-job green CI receipt nor a failed product assertion; the missing
coverage still requires a completed run on the final candidate.

## Fixed runtime candidate: `cb1d0632`

[Nightly 44932176472](https://github.com/cyjin-yl/shenko/actions/runs/44932176472)
checked out `cb1d06329fbea59ee3b1c80c4e91ebe396b4cde4`. This run completes the
Rust coverage missing above, but its compiler job has a separately diagnosed
upstream fixture failure. The complete Nightly is therefore not green.

### Rust long suite: passed

[Rust job 136930992996](https://github.com/cyjin-yl/shenko/actions/runs/44932176472/job/136930992996)
passed the full workspace command and all three later explicit proofs. Its
125 unit/integration targets and 33 doctest targets yield **1,239 passed,
zero failed and 11 ignored** across 157 final libtest summaries, excluding
nested child-process summaries. The remaining `cognition_self_learning`
target explicitly skips without the reference model/binary configuration;
this is still not real-model acceptance.

The repaired semantic-wait fixture explicitly reports `ok`; the four original
Node/artifact-directory failures also pass. The separate partition/recovery,
native-helper lost-reply and native-helper identity-refusal proofs all pass.
The [native evidence artifact](https://github.com/cyjin-yl/shenko/actions/runs/44932176472/artifacts/14719056648)
has ID `14719056648` and zip SHA-256
`b36e63fccba55f7958e21d0a7f2fc410ae1c66b9d46067ad4c8f105e1dd9a570`.

### Compiler: real assertions ran; one fixture ordering failure

[Compiler job 136930992702](https://github.com/cyjin-yl/shenko/actions/runs/44932176472/job/136930992702)
reported **182 passing files, one failing file and two skipped files**, and
**2,912 passing tests, one failing test and nine skipped tests**, in 560.65
seconds. The real Chromium sandbox preflight passed again: UID 1001,
Node `v24.22.1`, Playwright `1.58.2`, Chrome `157.0.8103.78`, namespace
isolation and Seccomp-BPF including TSYNC enabled. The hosted image version
was `20261116.133.1`; this changed image is recorded without assigning it the
cause of the test failure.

The sole failure is in the Happy DOM test
`packages/compiler/test/scroll-reveal-consumers.test.ts`, ordinary
`react-dom-source-v1`, rejecting an item from another surface, a sibling,
self and another family. Its four `unavailable` assertions passed, then
the after-`act` offset was 300 rather than 50. This test does not use Chromium.
The [compiler evidence artifact](https://github.com/cyjin-yl/shenko/actions/runs/44932176472/artifacts/14718537069)
has ID `14718537069` and zip SHA-256
`f189d5cd6529f95eb571f6d946a3baa4d0bfc3d6f8597fee4401cabc6b4ff350`.

An isolated checkout of the exact `f29738eb6a804ccfc58510cc5a5bd1692690716e`
pin retained the original 28 tests and every assertion. The unmodified file
passed twice. A traced, controlled ordering then reproduced the exact single
failure: four rejected requests each synchronously preserved 50, after which
a previously scheduled mounting end-follow frame called the actual emitted
`applyScroll(to-end)` and wrote 300. Completing those mounting callbacks before
injecting geometry passed all 28 tests, including the valid reveal's 50-to-70
movement. The original CI lacked setter traces; this is a reproduced legal
ordering with the same failure, not a replay of its original scheduler trace.

[Upstream PR #1671](https://github.com/Proto-UI/Proto-UI/pull/1671) carries
`795b1b4958333cf904547e484acb4de9d4b3ee80`, a direct child of the existing pin.
It repairs only test mounting order and retains the 29 original `expect`
expressions, matcher calls and expected values, the same 28 tests and enabled
end-follow policy. Its
[dated walkthrough](https://github.com/Proto-UI/Proto-UI/blob/795b1b4958333cf904547e484acb4de9d4b3ee80/internal/records/2026-11-22-compiler-scroll-fixture-order.md)
and
[machine evidence](https://github.com/Proto-UI/Proto-UI/blob/795b1b4958333cf904547e484acb4de9d4b3ee80/internal/records/evidence/2026-11-22-compiler-scroll-fixture-order.json)
retain the diagnostic patch, complete React trace excerpts, source hashes and
all seven local run receipts. The final formatted fixture passes 28/28.
This does not convert the failed Nightly into a pass or substitute for a new
complete compiler run at the adopted pin.

### Regular CI after migration conservation repair: six jobs passed

The earlier [CI 44932179743](https://github.com/cyjin-yl/shenko/actions/runs/44932179743)
failed migration conservation because the original `SPEC:21.12` block had
been edited. Three jobs passed; Rust and Clippy were cancelled after that
candidate was superseded. The repair restored the original block verbatim
and placed the current implementation note outside it, preserving the
original checksum and migration gate.

[CI 44932500777](https://github.com/cyjin-yl/shenko/actions/runs/44932500777)
then completed all six jobs successfully for the documentation repair
`2238f1040df9cac872aae9521819778b2b27a0b3`. Its actual PR merge checkout was
`d51c2bb1df52a159694457dd8e1471ddde0a12c9`, distinct from the Nightly runtime
checkout above. [Rust job 136932014015](https://github.com/cyjin-yl/shenko/actions/runs/44932500777/job/136932014015)
also records 1,239 passed, zero failed and 11 ignored, plus the explicit
unconfigured-model skip. Its
[native fixture artifact](https://github.com/cyjin-yl/shenko/actions/runs/44932500777/artifacts/14718419309)
has ID `14718419309` and zip SHA-256
`46dd939059ddb714693f8fae942bf6aff652680c48e1b49a2a421079715e83f7`.
These successful receipts belong to those exact source identities; later
runtime or pin changes require their own checks.

The later runtime candidate `9a7be2f88e4922b8b70ba13ec2078145129f5ce0`
also completed all six jobs in
[CI 44935471497](https://github.com/cyjin-yl/shenko/actions/runs/44935471497).
Its actual merge checkout was `de6e96bf914003e7f85eceaa5a144b6a69de7bfd`.
[Rust job 136941006687](https://github.com/cyjin-yl/shenko/actions/runs/44935471497/job/136941006687)
again records 125 unit/integration targets, 33 doctest targets, 1,239 passed,
zero failed and 11 ignored, with the same separate unconfigured-model skip.
The [Rust evidence artifact](https://github.com/cyjin-yl/shenko/actions/runs/44935471497/artifacts/14720205594)
has ID `14720205594` and zip SHA-256
`351193bcb827a1a1f3c36ecc1baa53b52b1cc34a7838a546662f7b7642b4e0b5`.
That run still uses the original `f29738eb6a80` compiler pin; it cannot certify
the subsequent fixture-pin adoption or its complete Nightly.


## Fixture-pin adoption: retained preparation attempts

The adopted upstream candidate is the direct fixture-only child
`795b1b4958333cf904547e484acb4de9d4b3ee80`; it does not bring in the unrelated
production changes on current upstream `main`. The new frontend generation
uses Shenko `9a7be2f88e4922b8b70ba13ec2078145129f5ce0` plus only the reviewed
`upstream-pin.json` change. The original build manifest is not edited by hand.
The producer and independent verifier each run the real 42-package public
closure, frontend type check, existing tests and build in the project
devcontainer. The successful execution below precedes adoption of its
original generated manifest.

Three preparation attempts failed before the public build and are retained:

| Attempt | Actual failure | Preserved receipt | Resolution for the next attempt |
| --- | --- | --- | --- |
| 1 | Frozen upstream installation could not `chmod` the host-owned `packages/cli/bin/proto-ui.js`; container and host exited 255. | [Original logs and hashes](frontend-pin-795b1b49-20261122/attempt-1/integrity.json) | Make genuine private Git checkouts on the container's persistent writable volume, retaining exact source identities and input hashes. |
| 2 | Git's linked-worktree upload-pack refused the host repository's resolved administrative Git directory as dubious ownership; both processes exited 128. No dependency install started. | [Original logs and hashes](frontend-pin-795b1b49-20261122/attempt-2/integrity.json) | Add only the two observed project administrative Git directories to this build process's `GIT_CONFIG` safe-directory list. |
| 3 | The standalone upload-pack preflight passed, but clone's actual upload-pack child still refused the same directory; both processes exited 128 before dependency installation. | [Original logs and hashes](frontend-pin-795b1b49-20261122/attempt-3/integrity.json) | Pass each exact directory through `git -c safe.directory=…` inside the actual `--upload-pack` command. Real `--shared --no-checkout` clones of both sources then passed and retained their expected HEADs. |

These preparation changes leave source ACLs, global Git configuration,
product expectations and shared default tooling intact. The
[process and actual-clone preflights](frontend-pin-795b1b49-20261122/preparation-checks.json)
retain the observed results. After each failure, an actual
container `/proc` scan for that attempt's unique paths found no remaining
process, and the retained container exit file independently confirmed its
termination. Each attempt uses the same shared build/model admission lock;
the container never reacquires the host's already-held lock.

At the 2026-11-21 19:14 UTC read, upstream
[PR #1671](https://github.com/Proto-UI/Proto-UI/pull/1671) remained draft and
open at this exact head. Its
[CI 44936359906](https://github.com/Proto-UI/Proto-UI/actions/runs/44936359906)
reported success for four configuration, plan and lockstep jobs, while all
18 build/test/release-related jobs were skipped. That result is not complete
compiler or browser coverage. The independent local source review and its
28-test receipt also do not claim a published approval or a completed final
Shenko Nightly.


### Actual devcontainer generation and independent rebuild: passed

Attempt 4 acquired the shared lock at **2026-11-21 19:32:45 UTC**. The actual
producer passed at 19:34:24; the original `scripts/ci/web_assets.py prepare`
and `check` completed their fresh independent checkout/rebuild at 19:36:09.
Both container and host exited zero at 19:36:10. The
[receipt index](frontend-pin-795b1b49-20261122/attempt-4/receipt-index.json),
[original comparison](frontend-pin-795b1b49-20261122/attempt-4/rebuild/comparison.json),
and [actual executed script](frontend-pin-795b1b49-20261122/attempt-4/container-build-web.sh)
bind this result to source, inputs, commands and output bytes.

The project devcontainer image is
`sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565`.
Both generations used Node `v24.22.1`, pnpm `10.32.1`, the unchanged locked
upstream dependencies and exact commit `795b1b4958333cf904547e484acb4de9d4b3ee80`.
The private Node installation left the shared container default untouched.

| Check | Producer | Fresh independent rebuild |
| --- | --- | --- |
| Actual public dependency closure | 42 packages built | Same 42 packages built |
| Frontend type check | Passed | Passed |
| Existing frontend tests | 51 passed; zero failed, cancelled or skipped | 51 passed; zero failed, cancelled or skipped |
| Static payloads | 12 complete files validated against generated manifest | Same membership and every payload byte identical |
| Compiler and generated Prototype provenance | Actual compiler source, bundle, resolutions and generated files validated | Stable manifest and all compiler/Prototype digests match; `errors: []` |
| Source conservation | All 53 frontend inputs and 13,502 tracked upstream inputs unchanged before/after | Same admitted input inventories; original checkout unchanged |

The [source summary](frontend-pin-795b1b49-20261122/attempt-4/source-input-summary.json)
retains the exact Shenko HEAD and pin-only diff. Its frontend inputs were
also compared with the local adoption worktree before copying any output.
All **12 served payloads are byte-for-byte identical to the preceding
`f29738eb6a80` pin's generation**. The committed frontend changes are the pin
and original generated provenance manifest; no payload or expected digest
is edited to satisfy the check.

The producer manifest copied verbatim into `crates/shenko-ui/src/static` has
SHA-256 `f9c51242b87e5201033c08b6439cfe6247cece4c45177feca092dcc67dbc0157`.
The [original independent manifest](frontend-pin-795b1b49-20261122/attempt-4/rebuilt-manifest.json)
has SHA-256 `bbac4693f08acea079224112426625d7fb462d8e0ec5dd8c8c355f3a46d51ecd`;
only the verifier's declared per-run timestamps, paths and receipt identity
are allowed to differ. The actual producer public-build receipt has SHA-256
`66af82f3017ef2856e15ab39f46274933bb483b063ee4b001df975d53adfedd1`, exactly
as recorded by the generated manifest.

The complete original archive, including all three full source-inventory
snapshots, is retained at
`/tank/shared/projects/shenko/build/daily-web-pin-795b1b49-r4/delivery.tar.gz`
and on the persistent container volume at
`/var/idw-target/daily-web-pin-795b1b49-r4/delivery.tar.gz`. It is 3,653,586
bytes with SHA-256
`a045c55dba49bd4efa2eea0d5d7ed0d720c4beee64e6b77ffe2cf9e9a1e6d77e`.
Its 56 transferred chunks and reconstructed archive were independently
hash-checked before adoption. The receipt index lists all 44 original file
hashes; this repository retains the compact original logs, actual build
receipts, comparison, scripts and manifest alongside it.

This is complete frontend generation/rebuild evidence for the stated inputs.
It does not certify a later Rust/runtime candidate, real-model task completion,
clean stop or the complete compiler suite on the new pin. The subsequent
CI and Nightly must retain their own checkout and artifact identities.


## Integrated checkpoint dc3f519c

[CI 44943382317](https://github.com/cyjin-yl/shenko/actions/runs/44943382317)
completed all six regular jobs successfully on PR head `dc3f519c048f3cff7ce851ace9323b2e673f377a`.
The actual synthetic merge was `399d81e8b579442ae3e2e0f703e0628adaa6a7e6`,
with base `f3fa6f4d8bf22cfd2baf490d9c3a08bd39af4d42`. This checkpoint predates
the native rows3/archive10 and process-wide stop changes; it is not their gate.

The [outer-harness audit](daily-readiness-20261122/ci-dc3f519c-rust-outer-audit.json)
binds the exact Rust job log and the source of nested process output. It finds
**1240 passed, 0 failed, 11 ignored** in 157 outer libtest summaries. Summing all
176 summaries would incorrectly add 19 child-process completions already covered
by their owning tests. The 33 doctest targets each contained zero cases. A separate
`harness=false` cognition self-learning executable explicitly skipped because
the real-model opt-in was absent; that skip is neither a passed case nor one of
the 11 libtest ignores. No real-model or complete Nightly claim follows from this CI.


## Host-stop checkpoint 0d25d77f

The new [Clippy job 136973435653](https://github.com/cyjin-yl/shenko/actions/runs/44946037112/job/136973435653)
failed its actual ratchet: `unused_variables` increased by one in production
and two in all-targets for the signal `select!` binding. The [diagnostic identity](daily-readiness-20261122/ci-0d25d77f-clippy.json)
retains the 19,258,866-byte original job-log hash and GitHub's uploaded artifact
identity. `c3f7b04c` follows the existing `_signal_result` binding convention
while still checking its `Result`, context and stop request; it adds no lint
allowance and changes no gate. The next candidate must obtain its own green CI.
