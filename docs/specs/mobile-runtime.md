# Mobile runtime — [§24.5](mobile-runtime.md#spec-24-5)、[§24.5.1](mobile-runtime.md#spec-24-5-1)、[§5.4](nodes.md#spec-5-4) (C15 mobile half)

**内核定位。** 参考插件，APK 只是发行包，建立在[内核合同](kernel.md)的[内核 5](kernel.md#kernel-5)、[内核 6](kernel.md#kernel-6)与[内核 7](kernel.md#kernel-7)之上。

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

## 2. Node portability ([§5.4](nodes.md#spec-5-4))

The node core never gains OS gates: `shenko-node` uses only `cfg(unix)`
APIs, `dirs_next` falls back cleanly, and mobile-only behavior lives entirely
in `shenko-mobile` behind the `DeviceBridge` seam. On non-Android hosts the
spawned handlers receive `UnavailableBridge`, so every mobile execution
returns the **typed unavailable fact** naming the exact missing mechanism —
a desktop node can never pretend to hold this phone's OS grants ([§5.4.4](nodes.md#spec-5-4-4): a
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
| `mobile.appdb.snapshot/v1` | [§24.5.1](mobile-runtime.md#spec-24-5-1) fact ladder (below). |

`drive()` is mirrored on the desktop computer-use backend: arm → execute →
T1 (evidence)/E1 (typed failure)/T2 (stale lease) recording via
`close_external_writer`, so partial failures keep their evidence path.

## 6. App-database facts ([§24.5.1](mobile-runtime.md#spec-24-5-1))

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
  the proof recipe in [§8](wasm-home.md#spec-8) remains required before claiming on-device support.
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

<a id="spec-24-5"></a>
<!-- migrated-contract: SPEC:24.5 -->
## 24.5 移动 OS 感知与执行均为插件，APK 是发行组合

2026-10-26 用户要求：“这种都是以插件形式实现的要。但是打包 apk 版的时候可以自动 bundle 上这些插件而已”。这是尚待实现和验证的设计，不声称当前 APK 或任何手机已经具备这些能力。

Android/iOS 权限感知、无障碍、通知与应用适配、文件/数据库观察、root 执行、Shizuku、Dhizuku 接入全部通过可安装、替换、停用和更新的插件实现；不为平台另造主体或将这些后端硬编码进核心。APK 可以默认 bundle 一组 Android 插件作为发行组合，仍保留单元身份和版本：**已 bundle ≠ 当前可用 ≠ 已获 OS 授权**。iOS 使用其真实开放的接口与权限，不能把 Android 的无障碍、通知、root 或管理接口直接视为 iOS 的等价能力。

Root 是用户偏好的可选增强，不是 Shenko 启动、加入或提供智能能力的前提。无 root 时继续使用实际可得的普通系统权限、应用接口、无障碍或其他适配；缺少某一路径不把整个 Node 判为不可用。插件观察当前 OS/应用版本、服务状态、实际身份和返回结果，记录可用、未知、失败及重新验证的条件。未知允许调查和尝试，不凭设备标签硬拒绝，也不声称真实系统限制已经消失。

| 接入插件 | 上游机制与本产品拟用范围 | 必须保持的区别 |
| --- | --- | --- |
| Shizuku adapter | 通过由 ADB 或 root 启动的服务，借 Binder 代理访问相应系统 API | ADB 与 root 身份能力不同，随系统版本和实际服务状态验证；不是所有应用数据的通行证 |
| Dhizuku adapter | 通过其 API 代理/共享 DeviceOwner 能力，用于实际支持的设备管理操作 | DeviceOwner 不是 root，也不等于 ADB；管理能力不自动带来任意应用私有数据明文读取 |
| Root / native execution adapter | 使用该节点实际存在的 root 能力进行观察和执行 | 获得 root 不证明每个加密库、硬件绑定数据或应用格式都能解析 |
| 普通权限、无障碍、通知及应用 adapter | 在对应 OS 实际提供的接口中取得观察或完成操作 | 各路径并列可组合，不假设层层升级即可得到全部能力 |

Shizuku 与 Dhizuku 不是“普通权限 → Shizuku → Dhizuku → root”的权限阶梯。具体接入、启动与授权状态由插件如实发现；可利用的机制与它们实际能做的事分别说明。上游机制静态核查于 2026-10-26：[Shizuku README](https://github.com/RikkaApps/Shizuku/blob/b844bc491f1790c72328e1a8e5b2349f8978f0ea/README.md)、[Dhizuku README](https://github.com/iamr0s/Dhizuku/blob/ac783ee7a5cfe4b87c1c07747c13b012c0f5822a/docs/README.md)。这些资料不是 Shenko 已完成集成的证据。

<!-- /migrated-contract: SPEC:24.5 -->

<a id="spec-24-5-1"></a>
<!-- migrated-contract: SPEC:24.5.1 -->
### 24.5.1 自有应用 SQLite 观察与自主风险判断

用户希望在自己的手机上尽可能读取应用数据，包括微信、QQ 等应用的 SQLite 数据库。将其保留为应用适配插件的设计目标；针对具体 OS、应用/数据库版本、进程身份、实际权限、加密与密钥可得性、文件可达性和解析能力逐项验证，不能承诺任意应用数据库均能明文读取。文件可读、可解密、结构可解释、语义完整是不同结果；没有取得正文时不伪报已经理解数据库内容。

适配器优先考虑低副作用的一致性只读快照或应用支持的导出，再在副本上分析；识别 SQLite WAL/并发写入等一致性因素，不能把随手拷贝一个活跃主库文件当作完整快照。可行时复用 [SQLite Online Backup API](https://www.sqlite.org/backup.html) 等一致性机制，方法仍须适配实际格式和可用连接。这是让主体按风险、收益、资源成本与后果自主选择的工程建议，不是禁止探索其他方法或新增人类审批的硬 gate。

观察保留采集时间、应用/格式版本、方法、缺失与不确定性，支持后续撤回误判和重新分析。可信本地节点可依既有同步合同共享认知，但复制数据或结论不证明接收端已经获得该手机的 OS 权限。此次设计修订没有读取私人数据库、提取密钥、安装插件或变更任何设备权限。

<!-- /migrated-contract: SPEC:24.5.1 -->
