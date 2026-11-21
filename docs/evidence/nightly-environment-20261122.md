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
| [Compiler full](https://github.com/cyjin-yl/shenko/actions/runs/44896952438/job/136824571668) | Ten suites failed at Chromium launch: running as root without `--no-sandbox` is unsupported. The run reported 173 passing files, two existing skipped files, 2,816 passing tests and 106 existing skipped tests. | The ten browser suites did not execute their product assertions. Their failure is not evidence of ten distinct compiler defects. |

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

These are configuration and preparation checks. The GitHub runner's browser
preflight and the full Rust/compiler suites still require execution on the
published integrated candidate. Their results must be appended with the run
URL and actual SHA; an environment repair must not turn the original red run
into a claimed pass.
