# Mobile runtime — SPEC §24.5 / §24.5.1 / §5.4 (C15 mobile half)

This document is the implementation contract for the Android half of C15:
mobile OS perception and execution are **plugins, not core business**, and the
APK is **only a distribution bundle**. Bundled ≠ available ≠ OS-authorized.

## 1. Composition

```
┌─────────────────────────── org.shenko.node APK ───────────────────────────┐
│ ShenkoNodeService (foreground, dataSync)                                    │
│  ├─ DeviceBridgeServer  — UNIX socket <filesDir>/device_bridge.sock        │
│  │                        + token <filesDir>/device_bridge.token (0600)   │
│  └─ node thread ─ JNI nativeStart(filesDir, port)                           │
│        └─ mobile/rust-shell cdylib (libshenko_android.so)                  │
│             └─ shenko::run_owned(Command::Start, filesDir)                 │
│                  = the SAME bin/shenko library target the CLI drives       │
└─────────────────────────────────────────────────────────────────────────────┘
```

- **No renamed-ELF exec, no forked runtime.** The node runs in-process as a
  cdylib loaded by the service; `bin/shenko` gained a library target
  (`src/lib.rs`, lib name `shenko`) exposing blocking `run_owned(Command,
  PathBuf)`. The thin desktop executable calls `run_cli`, which parses the
  CLI, initializes logging and enters `run_owned`. JNI enters `run_owned`
  directly. The shared host acquires or adopts the actual runtime-owner lock
  before building Tokio and retains it until Tokio has been dropped. Both
  hosts use the fully composed library, not a copied older runtime. This
  source arrangement does not establish Android lifecycle or device acceptance.
- **Data dir = app filesDir** (app-private). Store, CAS, owner lock, and the
  bridge socket all live under it. `--data-dir` remains the override on
  desktop; the APK always uses filesDir.
- `mobile/apk/bundle.json` pins artifacts, licenses, units, Rust targets.

## 2. Node portability (SPEC §5.4)

The node core never gains OS gates: `shenko-node` uses only `cfg(unix)`
APIs, `dirs_next` falls back cleanly, and mobile-only behavior lives entirely
in `shenko-mobile` behind the `DeviceBridge` seam. On non-Android hosts the
spawned handlers receive `UnavailableBridge`, so every mobile execution
returns the **typed unavailable fact** naming the exact missing mechanism —
a desktop node can never pretend to hold this phone's OS grants (§5.4.4: a
copied conclusion is not the receiver's capability). Mobile handler kinds are
advertised through the bundled unit's `provides` list
(`mobile/apk/bundle.json`); no `units.rs` change is needed.

## 3. Authorities — parallel, combinable, no ladder

| Authority | Mechanism (verified upstream) | Identity caveat |
|---|---|---|
| `root_su` | Exec-form `su 0 ARGV...` (AOSP/LineageOS su execs argv verbatim; su implementations that only accept a shell command string report `typed_unavailable` — constructing a shell string is forbidden) | uid 0; user-managed allowlist |
| `shizuku_binder` | `Shizuku.bindUserService(UserServiceArgs, ServiceConnection)` + `IShenkoShellService.execJson` (rikka.shizuku api 13.1.5, Apache-2.0) | ADB-started ≠ root-started; probe reports the binder uid, never assumes equivalence |
| `dhizuku_owner` | `Dhizuku.newProcess(cmd[], env[], dir)` (Dhizuku-API 2.7.0, MIT) | Device owner is not root and not ADB; grants no app-private-data read |
| `accessibility` | User-enabled `AccessibilityService`: gestures, node tree, human-activity event feed | Only what the service can observe/dispatch |
| `normal_perm` | This app's own uid surface | Baseline |

No "normal → Shizuku → Dhizuku → root" escalation path is encoded anywhere;
`auto` picks the strongest *granted* mechanism and reports which one was used
in `authority_used`.

## 4. Bridge protocol

- UNIX socket, filesystem namespace, same uid; one connection per round-trip
  so app-process death surfaces as a connect failure.
- Frame 1 (client→server): `{"version":1,"token":"..."}\n`.
- Server reply `{"ok":true}\n`, else `{"ok":false,"error":{kind,message,retryable}}`.
- Frame 2: one request JSON line, for example
  `{"version":1,"request_id":"...","op":"probe"}`, ≤ 64 KiB. The operation
  discriminator is a top-level string; operation arguments are siblings of
  `op`, not a second nested operation object.
- Frame 3: one response JSON line ≤ 16 MiB; errors carry `error.kind` in
  `typed_unavailable|denied|io|unsupported|timeout|internal`.
- Successful operation fields are at the response root alongside `ok`, for
  example `{"ok":true,"binding":{...},"authorities":{...}}`; there is no
  `result` wrapper. The Rust client retains those fields and the original
  error classification instead of replacing a successful response with null.
- Frames are UTF-8. The APK buffers bounded bytes until LF, then decodes the
  complete frame; splitting a multibyte code point across reads cannot alter
  text or argv. Malformed UTF-8 and a partial/oversized frame are not requests.
  The APK's response bound also counts encoded bytes, not UTF-16 characters.
- The filesystem listener is a bound `LocalSocket` wrapped by
  `LocalServerSocket(FileDescriptor)`. The latter borrows the descriptor;
  stop closes its retained `LocalSocket` owner, as required by the
  [Android API contract](https://developer.android.com/reference/android/net/LocalServerSocket).
- The token binds the **bridge service incarnation** (app process + service
  lifecycle). It is not a grant and never elevates any operation.

Ops: `probe`, `capability_list`, `screen_capture`, `input_batch`,
`shell_exec`, `db_snapshot`, `human_activity_after`. The Rust vocabulary is
the `BridgeOp` enum in `crates/shenko-mobile/src/bridge.rs`; Kotlin dispatch
is `DeviceBridgeServer`/`BridgeOps`.

All three shell mechanisms use the same process capture: stdout and stderr
are drained concurrently while the process runs, retaining at most 4 MiB of
each stream and discarding excess bytes while continuing to drain. Waiting
for exit before reading, or stopping the reader when retention fills, can
block an otherwise finite child on its output pipe. A normal completion keeps
its actual exit code; a process deadline reports `timed_out` and retires the
direct child. This is not proof that an arbitrary remote process implementation
or a detached descendant has terminated; device-side lifecycle acceptance
remains separate.

## 5. Plugin handlers (`crates/shenko-mobile`, UUID
`3b9c1a2e-7d4f-4e8a-9b15-6c2e8f0a1d34`)

| Kind | Honest contract |
|---|---|
| `mobile.device.probe/v1` | Facts only: authority states, OS/app versions, salted install-binding digest (never a credential). |
| `mobile.screen.observe/v1` | Sealed `MobileObservation` (PNG decode, pixel hash, bounds, secure flag). `revision` is a per-session monotonic u64 with the same comparability notion as the desktop computer-use revision. |
| `mobile.control.lease/v1` | Per-display-domain leases with epoch + takeover detection; epoch revocation invalidates queued input. |
| `mobile.control.input/v1` | Executes against a live, unrevoked lease; re-verifies grant; binds to a prior sealed observation by content digest. |
| `mobile.shell.exec/v1` | Frozen argv verbatim (no shell string), bounded output, timeout → `Interrupted`/Unknown effect. |
| `mobile.appdb.snapshot/v1` | §24.5.1 fact ladder (below). |

`drive()` is mirrored on the desktop computer-use backend: arm → execute →
T1 (evidence)/E1 (typed failure)/T2 (stale lease) recording via
`close_external_writer`, so partial failures keep their evidence path.

## 6. App-database facts (§24.5.1)

Separate, never merged: `file_located / file_readable / snapshot_recovered_copy
/ snapshot_consistent / plaintext_verified / structure_interpretable /
semantics_verified`, plus `KeyMaterialState` (key *material* availability is a
fact about material, independent of ciphertext readability).

- A raw db+wal byte copy is **never** `snapshot_consistent`.
- Only the rusqlite **backup API** on a source directly readable by the node
  uid can yield `snapshot_consistent` (the bridge returns
  `direct_readable_path` and the node runs backup itself), and only when no
  companion rollback image was copied.
- Otherwise the copied set is analyzed (SQLite header probe, `PRAGMA
  quick_check` through a readonly open) and reported at best as
  `snapshot_recovered_copy` with explicit concurrency uncertainty.
- Decryptability or semantic completeness is **never** claimed without the
  actual plaintext evidence. This plugin never extracts keys.

## 7. Honest gaps

- Host-side checks cover the Rust socket protocol and the actual shared Java
  frame/process-capture implementation on a native JVM. They do not exercise
  the Android service, the full APK build, Binder grants or a real device.
  The development container currently lacks the Android SDK/NDK and Gradle;
  the proof recipe in §8 remains required before claiming on-device support.
- `MediaProjection` pixel capture and app-supported export adapters are not
  bundled; without a shell-grade authority, `screen_capture` reports
  `typed_unavailable` rather than synthesizing pixels, and `app_export`
  reports `unsupported`.
- `shenko-node` network exposure on-device is the UI server port only; there
  is no outbound assumption beyond the existing channel adapters.

## 8. Native proof recipe (verification gate)

1. `cargo check -p shenko-mobile -p shenko-android-shell` and full workspace
   `cargo test` on desktop (CLI lib/bin both build; existing tests moved into
   the lib target keep passing).
2. `cargo ndk -t arm64-v8a -o mobile/apk/app/src/main/jniLibs build --release
   -p shenko-android-shell` (needs NDK + cargo-ndk).
3. With a compatible Gradle installed, run `gradle :app:assembleDebug` from
   `mobile/apk` (AGP 8.7.3, Kotlin 2.0.21, minSdk 26); the repository does not
   currently bundle a Gradle wrapper.
4. Install on an emulator/device (API 26+): start ShenkoNodeService, confirm
   the node thread reports startup, `device_bridge.sock` exists, and
   `shenko identity` over the UI port answers.
5. Probe path: run a subject step asking `mobile.device.probe` with no
   grants → every authority `typed_unavailable` with reason; grant Shizuku →
   probe shows `shizuku_binder` available with binder uid; execute
   `mobile.shell.exec` argv `["id"]` → sealed record with exit code and
   bounded output.
6. Adversarial: revoke Shizuku mid-lease → queued input reports
   `external_takeover_revoked_lease`; db snapshot of an app whose db is not
   directly readable → `snapshot_recovered_copy` with uncertainty, never
   `snapshot_consistent`.
