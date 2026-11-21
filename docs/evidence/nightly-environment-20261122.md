# Nightly environment repair — 2026-11-22

This change repairs the execution environment of the existing complete suites.
It does not change Rust assertions, compiler expectations, the reviewed
Proto-UI source pin, or the set of selected test files. A subsequent successful
run must still be tied to its own candidate; this repair alone is not a green
Nightly receipt or real-model daily-use acceptance.

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
