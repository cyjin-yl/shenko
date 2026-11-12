# Desktop runtime contract (computer.use, OS/VM providers, original LayerFS)

**内核定位。** 参考插件，登记在能力架上，建立在[内核合同](kernel.md)的[内核 5](kernel.md#kernel-5)、[内核 6](kernel.md#kernel-6)与[内核 7](kernel.md#kernel-7)之上。操作系统与虚拟机提供者各自可以替换；没有桌面会话或权限是事实，不是内核的禁令。

Scope: [§10.2.1](durable-state.md#spec-10-2-1)、[§22](nodes.md#spec-22)、[§24](desktop-runtime.md#spec-24)、[§34](desktop-runtime.md#spec-34); work package C15 (ArchDesktop).
Sources: `crates/shenko-exec-host/src/desktop.rs` (neutral contract),
`computer_use.rs` (product + X11 provider + native task kinds),
`desktop_windows.rs`, `desktop_macos.rs` (real FFI providers),
`layerfs.rs` + `scripts/layerfs-companion.mjs` (original LayerFS VM adapter),
registration in `bin/shenko/src/main.rs`.

## Kinds and callers

Six Cordis-registered native task kinds share one `ComputerUseBackend`
(store + provider registry + session map + human-input fanout + LayerFS
adapter). Registration is real in `bin/shenko/src/main.rs`:

| kind | handler | verb surface |
| --- | --- | --- |
| `computer.use.session` | `ComputerUseSessionHandler` | `{"backend":"x11"\|"windows"\|"macos"\|"auto","display":…,"screen":…,"op":"open"\|"close"}` |
| `computer.use.observe` | `ComputerUseObserveHandler` | `{"session_id","region":[x,y,w,h]\|null}` |
| `computer.use.lease` | `ComputerUseLeaseHandler` | `{"session_id","op":"acquire"\|"release"\|"takeover","lease_token"?}` |
| `computer.use.input` | `ComputerUseInputHandler` | `{"session_id","lease_token","observation_id":"blake3:…","actions":[…],"require_pixel_fresh"?}` |
| `computer.use.capabilities` | `ComputerUseCapabilitiesHandler` | `{"backend":…\|null}` |
| `computer.use.vm` | `ComputerUseVmHandler` | `{"op":"start"\|"stop"\|"state"\|"commit_receipt","name":"vm-name"}` |

Every operation is ledger-armed by `drive()`: frozen input sealed before any
external effect, resource binding + acceptance + no-retry policy sealed,
receipts only from what the transport actually answered.

## Neutral provider contract (`desktop.rs`)

`DesktopProvider` (`open`/`observe`/`lease_op`/`inject`/`close`/`reconcile`/
`capabilities`) + `DesktopSession` (`session_id`/`provider_id`/`domain_id`/
`display`/`is_closed`). Shared types: `DesktopDisplay` (DPI, scale factor,
depth, capability facts), `DesktopFrame` (raw provider-native pixels + RGB8
preview + `frame_id` + per-session monotonic `revision` + `lease_epoch` +
layout fingerprint), `DesktopObservation` (sealed record; its blake3 digest
is the `observation_id`), `DesktopAction`/`DesktopPress` (serde-tagged, same
JSON the X11 kinds always accepted), `DesktopLeaseState` + `apply_lease_op`
(the single acquire/release/takeover algorithm every provider uses, with the
epoch bump and machine-held-input release atomic under the domain lock —
handover fence), typed `DesktopError::{Unavailable,Rejected,Lost}`.

Domain identity spelling is owned here and shared with the browser/mobile
peers by contract: `x11/{socket_endpoint_key}/root{root:08x}` where
`socket_endpoint_key` is `tcp:{peer}` or `unix:path:{hex}`/`unix:abstract:{hex}`
of the live X11 connection.

## Observation identity, revision, DPI ([§24.3](desktop-runtime.md#spec-24-3))

- `revision` is per-session monotonic, starts at 1; every published
  observation increments it (ordering without pixel compares). `frame_id` is
  the provider's capture counter.
- Observation record carries `session_id`, `provider`, `domain_id`,
  `display_id`, `screen_num`, `frame_id`, `revision`, `captured_at_ms`,
  `lease_epoch_at_capture`, `region`, `viewport`, `dpi_x/y`, `scale_factor`,
  `coordinate_transform`, `layout_fingerprint`, `pixel_hash` (blake3 of the
  sealed raw pixels), `pixel_format`, `raw_pixels_ref`, `png_preview_ref`.
- The product seals raw pixels (ZPixmap/BGRA/RGBA provider bytes), the PNG
  preview, and the record; the record digest is the durable `observation_id`.
- Input must carry the `observation_id`; the product re-reads the sealed
  record (CAS), checks identity + the 300 s freshness window, and the
  provider re-verifies the live layout fingerprint (and exact pixels under
  `require_pixel_fresh`) plus the lease epoch before any event. Stale
  coordinates are never retargeted.

## Write lease, takeover, cancel, reconcile ([§24.3](desktop-runtime.md#spec-24-3)、[§10.6](durable-state.md#spec-10-6))

- One lease per interactive display domain; multiple read-only observers
  are free. Lease binds the controller instance, not a finite task id.
- `takeover` always revokes the old epoch (late machine input rejected) and
  releases machine-held keys/buttons before the new writer can emit.
- The X11 watcher (one LIVE thread per domain, only when XI >= 2.1 was
  negotiated AND raw selection ACKed on its own connection) revokes on any
  non-XTEST XI2 raw input, releases/restores, then dispatches the domain id
  through `HumanInputFanout` so the browser provider drops its agent lease
  on the same domain (integration: Main wires the 3-line adapter impl +
  `register_human_input_observer` in main.rs; signature `on_human_input(&self,
  domain_id: &str)`).
- Input batches call the product's durable cancel probe (`store.get_task`,
  committed `abort_request`) before every atomic send unit; cancellation
  releases held inputs and reports partial facts (`result:"cancelled"`),
  never replay.
- Reconcile never replays: armed ops without conclusive evidence stay
  Unknown with in-memory progress facts (`sent_events_before_recovery`,
  `session_alive_in_memory`, `process_stop_proven:false`).

## X11 provider (source-wired)

x11rb 0.13.2, real `RustConnection`. Open probes XTEST + XInput2 versions,
screen geometry, DPI (mm → dpi), display domain id; session JSON preserves
the historical top-level names (`xtest_version`, `xinput2_version`,
`xtest_source_device_ids`, `domain_id`, …). Observation is real `GetImage`
(ZPixmap) + window-tree layout fingerprint; input is real XTEST
`FakeInput` with per-event lease re-checks, server grab around keymap
resolution, keycode remapping for unmapped keysyms with verified restore,
held-input tracking that never adopts foreign holds, and `send_log` for
recovery. Non-X11 backends and unreachable displays fail typed-unavailable
naming the exact missing capability; no human desktop is ever guessed.

## Windows / macOS providers (real FFI, no external OS crates)

Both are cfg-gated real implementations against the system APIs:

- Windows (`desktop_windows.rs`): `user32`/`gdi32` hand-rolled FFI.
  Capture: `GetDC`/`BitBlt`/`GetDIBSection` top-down 32bpp BGRA; input:
  `SendInput` with VK/scancode/UNICODE events; lease per input desktop
  (`OpenInputDesktop` name). Human-input detection honestly reports
  `available:false` (needs an LLMHF hook message loop owned by this
  process); takeover stays lease-bound.
- macOS (`desktop_macos.rs`): CoreGraphics/ApplicationServices FFI.
  Capture: `CGDisplayCreateImage` + data provider bytes (BGRA8), with crop
  to region; input: `CGEventPost(kCGHIDEventTap)` mouse/keyboard events,
  unicode via `CGEventKeyboardSetUnicodeString`; lease per CG display.
  Screen Recording grant absence is a typed-unavailable naming it;
  Accessibility trust is reported per session.

## Original LayerFS on a qualified Linux VM ([§10.2.1](durable-state.md#spec-10-2-1)、[§34](desktop-runtime.md#spec-34))

`layerfs.rs` + `scripts/layerfs-companion.mjs` never re-implement the
LayerFS Store. The pinned ORIGINAL SDK `v1.0.0@2aaf2552…` is bound at
runtime by export-surface probe (`Store` required; `Branch`/`Workspace`
reported); mismatch stops the adapter typed. A per-VM Node companion drives
a qualified Linux VM provider (BoxLite optional first candidate) and runs
the upstream SDK in the guest over the provider's exec channel. The
`computer.use.vm` kind is the registered caller: start/stop/state mutate or
read the named VM only; `commit_receipt` returns the upstream
`layerfs_committed` receipt and never `product_durable`. Guest capability
facts (Docker/FUSE/mounts for the full workspace path) are probed, not
assumed. Missing SDK path or provider is typed-unavailable open evidence,
not completion. Env: `SHENKO_LAYERFS_SDK_PATH`, `SHENKO_LAYERFS_VM_PROVIDER`,
`SHENKO_LAYERFS_NODE`, `SHENKO_LAYERFS_COMPANION_SCRIPT`.

## Native proof recipe (parent-serialized)

On a host with a real X server (`Xvfb :99` qualifies as a real X11 server):

1. `DISPLAY=:99 cargo run -p shenko` — six kinds registered.
2. `computer.use.capabilities` `{}` → registry lists `x11`.
3. `computer.use.session` `{"backend":"x11","op":"open"}` → session JSON with
   probed XTEST/XInput2 + DPI.
4. `computer.use.observe` → `observation_id` + sealed raw/preview/record.
5. `computer.use.lease` acquire → token; `computer.use.input` with a
   pointer_move/key/text action bound to the observation → receipts with
   per-action transport; `pointer_post` in the result.
6. Type into the X server with a second client while the lease is held →
   lease revoked, held inputs released, human-input fanout dispatched.
7. `computer.use.vm` with unset env → typed-unavailable naming the missing
   SDK path/provider (honest open evidence); with a pinned SDK checkout and
   a qualified VM provider, start/stop report the provider's real state.

Not yet executed here (source phase; parent serializes integrated native
verification): all of the above, plus Windows/macOS builds
(`cargo check --target x86_64-pc-windows-msvc`, `cargo check
--target aarch64-apple-darwin`) for the cfg-gated providers.

<a id="spec-24"></a>
<!-- migrated-contract: SPEC:24 -->
# 24 Computer Use 与完整操作系统节点

<!-- /migrated-contract: SPEC:24 -->

<a id="spec-24-1"></a>
<!-- migrated-contract: SPEC:24.1 -->
## 24.1 正式能力及插件边界

Computer Use 是正式规划中的执行能力。WASM 是 Agent 的基础家园；Agent 可以接入用户给予的完整 Linux、Windows、macOS 环境。具体环境可以是物理机器、受支持的 VM、桌面会话、远程桌面，或有适当显示服务的 Linux 容器。路由器/Android 控制节点可以仅安装轻型 transport，把桌面与重型视觉模型放在别的节点。

BoxLite ComputerBox / BrowserBox 列为 Linux 桌面与浏览器的优先验证候选，细节见 [§34](desktop-runtime.md#spec-34)。本地 helper 与远端 REST 存在具体能力差异，需 companion 或 tunnel adapter；观察版本、输入 lease、人工接管及原生 Windows/macOS provider 继续按本章合同实现。

ComputerUseProvider 提供节点发现与能力协商、建立/恢复会话、观察、输入、订阅变化、接管/释放和效果核验。它与 RuntimeProvider、ExecutionProvider、CredentialProvider、RemoteAgentProvider 共用节点/任务/身份体系；GUI 桌面、浏览器 context 和 tmux pane 保留各自准确的输入域。主体可以委派任意已有 GUI 能力；点击、输入、浏览器操作和文件传输由子 Agent 执行。

| 插件后端 | 能力 | 不能假设的等价关系 |
| --- | --- | --- |
| Native node companion | OS 截图、窗口、键鼠、可用 accessibility、文件与进程 | 有 SSH 权限不代表已获屏幕或输入权限 |
| VNC/RFB | framebuffer 与输入 | 像素坐标不等于稳定元素身份，传输成功不等于业务成功 |
| RDP | 对应服务器提供的远程桌面会话 | 不保证就是人类物理显示器上的同一个 session |
| Browser protocol | DOM、浏览器 AX、截图、tab/context 操作 | 浏览器控制不自动获得整个 OS 的 GUI 控制权 |
| SSH / agent seat | CLI、tmux、Claude/omp SDK/RPC | 终端可控不等于桌面可见；可关联同任务，协议仍分开 |

优先使用可得的结构化目标，保留视觉路径；DOM、AX 和 screenshot 都是观察方式，不能只提供其中一种就宣称全部桌面软件可控。扩展接口声明真实子能力，例如 screen_read、accessibility_read、input、clipboard、file_transfer、browser_context。媒体编解码、浏览器与 OS companion 可卸载，默认内核不常驻这些依赖。

<!-- /migrated-contract: SPEC:24.1 -->

<a id="spec-24-2"></a>
<!-- migrated-contract: SPEC:24.2 -->
## 24.2 操作系统边界

Windows containers 不能提供任意 GUI 桌面应用会话；完整 Windows 桌面应使用合适的 Windows VM、远程桌面或机器。macOS 则需要受支持的 Mac 环境；Apple silicon 的 Virtualization 路线可作为 VM provider 参考，不能称为通用 Linux OCI 镜像。[Microsoft Windows containers 文档](https://learn.microsoft.com/en-us/virtualization/windowscontainers/quick-start/lift-shift-to-containers)、[Apple macOS VM 文档](https://developer.apple.com/documentation/virtualization/running-macos-in-a-virtual-machine-on-apple-silicon)

macOS 屏幕录制与 Accessibility 授权分别处理；其他 OS 也按实际 session/权限报告。首次接入时说明真正缺少的 OS 能力；已有效授权后正常自主操作，不把每一次点击变成人工确认流程。[Apple 屏幕访问](https://support.apple.com/guide/mac-help/control-access-screen-system-audio-recording-mchld6aa7d23/mac)、[Apple Accessibility](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac)

<!-- /migrated-contract: SPEC:24.2 -->

<a id="spec-24-3"></a>
<!-- migrated-contract: SPEC:24.3 -->
## 24.3 观察版本和桌面输入所有权

观察携带 node/session/display/window/tab 中适用的稳定 ID、frame ID、时间、页面/布局 revision、viewport、DPI/缩放与坐标转换。窗口移动、页面导航、显示器变化或断线恢复会使旧观察失效；不能将旧坐标点在新画面上。视觉模型结果必须绑定它看过的观察，而非无来源坐标。

写入 lease 以共享焦点、鼠标和键盘的整个交互桌面域为单位；多个窗口通常共享这个域，不能各发一个 lease 后让两个子 Agent 抢鼠标。允许多只读观察者；隔离桌面 session 可独立写入。lease 只约束本产品及配合的节点，不声称可以阻止人类或未受管程序直接输入。

人类明确接管时撤销机器输入，旧 controller epoch 的延迟操作失效；恢复前申请新 lease 并重新观察。断线或取消时释放插件仍持有的键/鼠标按钮，避免修饰键持续按下。保存任务进度，不把暂停操作误记为任务失败或已完成。

<!-- /migrated-contract: SPEC:24.3 -->

<a id="spec-24-4"></a>
<!-- migrated-contract: SPEC:24.4 -->
## 24.4 GUI 副作用和资源

GUI action 记录意图、发送尝试、传输 ACK、可观察效果、应用级 receipt 与 unknown 状态。点击“提交”后断线，先观察实际结果，不盲目重放；VM/文件回滚无法撤回已发送消息或第三方提交。UI 操作 ID 不能单独保证 exactly-once。

视觉模型、模型 OCR、截图总结、远端 browser agent 与桌面 agent 使用同一受控本地模型时，全部进入资源 broker。变化检测、按需截图、裁剪和局部观察降低成本；图像 hash 命中不能被记为模型 KV 命中。记录可得的图像 token/prefill/费用信息，未知值不伪造。

屏幕、网页、AX、剪贴板和终端文本都带 scope 与来源。只有经任务许可的截图与结果才能进入主体或模型 provider；桌面内容里的指令不改变当前用户合同。多模态观察仍进入同一个主体 mailbox，视觉 worker 不是另一个永续人格。

<!-- /migrated-contract: SPEC:24.4 -->

<a id="spec-34"></a>
<!-- migrated-contract: SPEC:34 -->
# 34 BoxLite：可选完整 Linux Runtime 插件评估

<!-- /migrated-contract: SPEC:34 -->

<a id="spec-34-1"></a>
<!-- migrated-contract: SPEC:34.1 -->
## 34.1 结论与研究基线

BoxLite 列入**优先验证的可选完整 Linux RuntimeProvider / Computer Use 后端**。这是对用户提供线索的研究结论，尚未决定必须采用，也未开始开发。它能为 native Linux 工具、编译、自有远端节点、现成 coding agent、浏览器和桌面提供现成执行环境；基础 WASM 家园、真实 WASM SSH/gh/tea、Python/Node/Bun WASM 等目标继续按各自验收条件推进。

固定阅读主线为 `b59b8c1f39e45e49adede56a65749a431d1380e7`，提交时间 2026-10-22 14:08:07 UTC；独立查询的最新正式发布为 [v0.10.7](https://github.com/boxlite-ai/boxlite/releases/tag/v0.10.7)，发布于 2026-10-18 00:37:38 UTC。下文“当前源码”指该固定主线，不能自动视为 v0.10.7 发行包的能力。研究方法是阅读官方文档、源码与 release 元数据；没有安装、启动、测 RSS 或运行性能基准。[固定研究树](https://github.com/boxlite-ai/boxlite/tree/b59b8c1f39e45e49adede56a65749a431d1380e7)

<!-- /migrated-contract: SPEC:34.1 -->

<a id="spec-34-2"></a>
<!-- migrated-contract: SPEC:34.2 -->
## 34.2 架构位置与插件划分

BoxLite 的当前生产执行链为宿主 Rust runtime → shim 子进程 → libkrun microVM → Linux guest agent → OCI workload；Linux 使用 KVM，macOS 使用 Hypervisor.framework。它拥有独立 Linux 内核。OCI 是镜像分发与工作负载接口，不是 WASI ABI，也不表示任意 ISA 或任意 guest OS。仓库中的新 VMM、内存快照、Windows WHP 等设计进度需与当前生产实现区分。[架构](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/contributing/architecture/README.md)、[新 VMM 设计](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/contributing/architecture/vmm/README.md)、[镜像平台选择](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/images/store.rs)

建议将对接分为下列可独立替换的插件能力；这些名称表示接口职责，不要求新增永久内核模块。

| 插件能力 | BoxLite 可以承接的部分 | 本产品仍负责的合同 |
| --- | --- | --- |
| RuntimeProvider / ExecutionProvider | 本地或自有节点的 Linux Box、exec、PTY、文件传输、资源配置 | task/scope、取消回执、runtime generation、真实平台能力 |
| WorkspaceStore adapter | QCOW2 磁盘 snapshot/clone/export/import | checkpoint 保证等级、外部挂载边界、分支产物提升、跨域 GC roots |
| ComputerUseProvider / BrowserProvider | Linux 桌面截图与键鼠、Playwright/CDP 等协议 | 观察版本、输入 lease、人类接管、结构化能力与坐标有效性 |
| RemoteAgentProvider | Box 中 Claude Code、OMP 等运行环境与终端通道 | seat 定位、语义事件、选择题决策、联系人策略、唯一主体 |
| REPLProvider | 完整 Linux 中的 Python、Node、Bun 和原生依赖 | 会话 journal、依赖锁、对象恢复等级和副作用去重 |
| NetworkPolicy / UpdatePolicy adapter | Box 网络、镜像来源、运行库与插件版本配置 | 主体自主决策、worker 实施、效果核验、失败恢复 |

默认核心不强制链接或常驻整套 VM 栈。大设备可安装本地 adapter/companion；路由器和 Android 上的轻节点可控制自有远端 BoxLite 宿主，无需把 BoxLite Cloud 作为基础依赖。BoxLite 插件、适配器、运行库及构建配方都可被 Agent 自主更新或重写，遵循 [§9](self-update.md#spec-9) 的候选验证、交接和恢复过程，不引入永久不可修改的层。

**逻辑子 Agent、执行环境和模型推理许可分别计数。** 普通搜索/研究分支可复用已有环境或只持有逻辑 workspace；需要独立系统状态、实验隔离或文件分支时才新建/clone Box。一个 Box 内多个 exec 不代表文件相互隔离。Box CPU/RAM 配额不替代 [§13](inference.md#spec-13) 的推理并发 C、KV 驻留 K 与总预算；Box 内 harness、视觉模型和辅助模型若调用同一受控模型后端，必须进入同一 broker。尚未纳管的外部模型调用须标明，不能宣称硬全局覆盖。主体仍只提出意图与委派，不直接取得 Box exec/文件/REPL 工具。

完整 Linux 环境可以降低部分 WASM 移植工作的紧迫性，native 成果仍不能勾选真实 WASM 条目。LayerFS 已按用户决定走原版真实 Linux VM 集成，WASM 状态独立设计，见 [§10.2.1](durable-state.md#spec-10-2-1)。BoxLite 组合需验证 Store 的同 kernel/本地文件系统与 Docker/FUSE 条件；支持 OCI 并不直接证明该组合已通过，若不满足则使用其他合格 VM provider。

<!-- /migrated-contract: SPEC:34.2 -->

<a id="spec-34-3"></a>
<!-- migrated-contract: SPEC:34.3 -->
## 34.3 持久化、快照与任意挂载的真实边界

| 事项 | 固定源码事实 | 本产品接入要求 |
| --- | --- | --- |
| 保留磁盘 | 本地默认 `auto_delete=None` 回退到 `auto_remove=true`；stop 默认删除 | 持久 Box 显式设置 `auto_delete=0`，不照抄临时 demo 的 cleanup 默认 |
| 脱离创建者 | `detach` 控制创建者 runtime 退出后的运行；与磁盘保留是不同选项 | 分别定义创建者退出、stop/start、宿主重启及重新接管；不声称断电保留活进程 |
| Snapshot / restore | 当前保存 container disk；restore 拒绝 active Box，需先 stop | 声明 disk-only，恢复后进程/REPL 按自己的协议重建；不能报 RAM/socket/KV resume |
| Clone | 共用 container base，再建薄 COW overlay；新 Box 从停止态开始 | 重新绑定 task、workspace、generation 和 lease；不继承旧 worker 的执行权 |
| Export / import | flatten 磁盘链，manifest 与 disk 打包为 `.boxlite` | 记录架构、image/runtime/guest 依赖；不把单个顶层 QCOW2 当完整迁移闭包 |
| 本地挂载 | 现有 `VolumeSpec.volumes` 支持 host 文件/目录 RO/RW | 路径属于对应节点且受真实宿主权限约束；继续支持任意可访问路径目标 |
| REST 挂载 | 所查 client 拒绝 host bind 和只读 managed volume；新 typed mounts 在本地/REST 均未实现 | 远端任意宿主路径由节点本地 adapter 兑现；不静默降级 RO，不把控制端路径当服务端路径 |

依据：[生命周期与默认值](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/options.rs)、[snapshot/restore](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/local_snapshot.rs)、[snapshot manager](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/snapshot_mgr.rs)、[clone/export](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/clone_export.rs)、[本地 mounts 限制](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/rt_impl.rs)、[REST 能力限制](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/runtime.rs)。

当前 quiesce 先尝试 guest filesystem freeze，再暂停 shim；freeze RPC 存在 best-effort 降级。因此 snapshot 成功不自动等于所有应用完成 checkpoint，也不构成跨宿主目录与控制数据库的事务。Adapter 能观测降级时须记录；无法判定时采用较低保证等级，不擅自宣称应用一致性。需要更强一致性的 SQLite/Git/REPL 工作负载，由 worker 先完成相应 checkpoint。断电持久性仍待目标文件系统故障验收。[quiesce 实现](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/box_impl.rs)、[guest freeze](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/guest/src/storage/fsfreeze.rs)

**磁盘 clone 不会把宿主共享目录自动分叉。** 克隆配置可能继续指向同一个 host path，因此多方案研究必须另建 worktree/COW workspace/独立目录，或显式声明共享。默认 host-mounted Git 记忆继续采用 [§10](durable-state.md#spec-10) 的工作树与发布合同。主事件账本、已确认消息、身份/撤销、外部副作用与正在进行的更新交接不随 Box 工作区回退；恢复需重新取得有效 lease。QCOW2 不提供本产品所需的跨 runtime CAS、语义冲突合并或全局 GC，仍由 [§10](durable-state.md#spec-10) 的可替换 storage 层统筹。

凭据也须按实际数据流处理：`Secret.value` 在所查源码中参与宿主 DB/管道序列化；export 将 BoxOptions 放入 manifest。由这条调用链可推断，包含此类配置的归档可能携带真实 secret，Debug 隐藏不等于归档加密。本轮未用真实凭据生成归档。Adapter 应采用受控归档、过滤/重绑定 secret 与 host path；不能默认把它提交进普通记忆 Git。[Secret 与序列化](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/options.rs)、[export manifest](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/clone_export.rs)、[archive 打包](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/litebox/archive.rs)

<!-- /migrated-contract: SPEC:34.3 -->

<a id="spec-34-4"></a>
<!-- migrated-contract: SPEC:34.4 -->
## 34.4 Computer Use、远端 Agent 与 REPL

ComputerBox 在 Python 和 Node SDK 中有真实实现，默认是 Ubuntu XFCE Linux desktop，提供截图、键鼠、拖拽、滚动等原语。BrowserBox 可使用 Playwright 的 Chromium/Firefox/WebKit，另有 Chromium CDP / Firefox BiDi 路径；模式和 engine 能力分别协商。它们足以作为 [§24](desktop-runtime.md#spec-24) 的具体候选，不能由“宿主支持 macOS/WSL2”推成原生 macOS/Windows 桌面 guest。[ComputerBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/computerbox.py)、[BrowserBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/browserbox.py)、[官方 Box 类型说明](https://docs.boxlite.ai/manage-sandbox/sandbox-types)

所查高层 GUI helpers 自动配置 host ports，BrowserBox 生成 localhost endpoint，而 REST runtime 拒绝这些 host ports。因此“换一个 runtime 参数就远端透明可用”尚不成立；建议验证节点 companion 使用本地 helper，或底层远端 Box + network tunnel 的专门 adapter。该结论是两侧实现约束推导，未做运行复现。截图尺寸须依据实际观察，不依赖 helper 的默认常量；[§24](desktop-runtime.md#spec-24) 的 frame revision、桌面输入 lease、人工接管与未知副作用合同仍需适配。[REST ports 拒绝路径](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/runtime.rs)、[Node GUI helper](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/node/lib/computerbox.ts)

`boxlite serve` 已有真实 REST 服务实现，可提供 Box、exec/PTY、文件和快照相关操作；服务端能力与 API 版本须分别验证。固定主线的 guest SSH control 已有 Rust 与本地 serve REST 实现，但其他部署可能不支持，且该能力没有专门的协商机制。它配置的是 guest SSH server，不能替代 Agent 的 SSH client 身份或 ssh-wasm 目标。[serve](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/cli/src/commands/serve/README.md)、[SSH 控制](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ssh.md)

现成 Claude/SkillBox 示例可用作有限执行 seat 的参考；个人主体、人格、群聊、记忆、全局 broker 和自主联系用户逻辑继续归属本产品。Linux Python/Node/Bun 能安装原生包，但活 REPL 对象、socket 和外部句柄的恢复仍按 [§19](repl.md#spec-19) 分级。调用方超时或取消 coroutine 不自动证明 guest process 已终止；adapter 必须执行实际取消并核验终止/unknown 状态，再释放相应执行容量。[Agent 集成与取消说明](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ai-agent-integration.md)、[SkillBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/skillbox.py)

<!-- /migrated-contract: SPEC:34.4 -->

<a id="spec-34-5"></a>
<!-- migrated-contract: SPEC:34.5 -->
## 34.5 自主联网、代理与升级

接入 [§9.4](network-observation.md#spec-9-4) 的同一条自主决策链：网络问题、可用更新或配置选择进入主体；主体自行选择是否更新、直连或代理；worker 检验并实施；已授权范围内不增加逐次人工批准。BoxLite 需要至少区分三条网络路径：

| 路径 | 必须单独描述的配置范围 |
| --- | --- |
| 宿主下载 OCI image | registry 认证、CA、pull proxy；远端 Box 用服务端配置 |
| guest 程序联网 | 程序/协议代理、DNS、NIC/egress policy；不自动继承镜像下载 proxy |
| 控制面与模型 API | 节点连接、broker/provider 路由；不因换 proxy 重新计算物理后端或释放旧请求的 C |

固定主线已有 `image_registry_proxy` 与相应校验；它仅覆盖镜像 pull。显式配置与环境变量有特定覆盖关系，支持的 proxy URL scheme 也需按版本读取。**v0.10.7 的 options 中未见该字段，专门模块在该 tag 不存在**，故不能把当前主线的该接口承诺成正式包已支持。默认 gvproxy 是 Box 网络栈的一部分，不能只因名称含 proxy 就当成用户指定的 HTTP/SOCKS 上游代理。[主线 image proxy 文档](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/image-registry-configuration.md)、[主线校验模块](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/images/image_registry_proxy.rs)、[v0.10.7 options](https://github.com/boxlite-ai/boxlite/blob/v0.10.7/src/boxlite/src/runtime/options.rs)、[网络架构](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/concepts/networking.md)

主线 outbound 更新是整方向替换而非 merge；既有连接与新连接的策略生效时机不同。创建时没有 NIC 的 Box 不能凭 runtime update 获得热插网卡；成功持久化状态也不替代实际可达性探测。Adapter 应保留 desired/applied/observed 区分，按目标流量核验；配置断链后由预先准备的本地恢复机制收敛，不依赖还能访问 LLM。这里的恢复计划是执行机制，不替主体决定要不要更新或使用代理。[网络更新语义](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/networking.md)

<!-- /migrated-contract: SPEC:34.5 -->

<a id="spec-34-6"></a>
<!-- migrated-contract: SPEC:34.6 -->
## 34.6 设备、体积与资源核算

官方支持的主要宿主为 Linux x86_64/ARM64 + KVM、Apple Silicon macOS + HVF，以及具备相应 KVM 条件的 WSL2 路径；不能由 ARM64/Linux 内核推导 Android 或各类路由器现成可用。Android target 在当前 system check 中尚不受支持；OpenWrt/musl/MIPS/32 位等需要单独移植验证。README 的 macOS 12+ 与 Python wheel 的 macOS 14.0 构建 target 有差异，应按实际安装包核验。[平台矩阵](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/README.md)、[system check](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/system_check.rs)、[Python 打包](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/pyproject.toml)

生产代码未指定资源时采用 1 vCPU / 1024 MiB guest memory；这不是 RSS 或最小运行需求。v0.10.7 Linux x64 runtime 压缩包为 28,138,146 bytes，约 26.8 MiB，也不含全部 image/workload，更不等于完整 Agent 内核体积。正式接入分别报告控制框架、BoxLite runtime/shim、guest 工作集、image/base cache、clone/export 峰值与外接模型服务的开销；启动/停止/并发数不沿用未经目标设备验证的宣传指标。[资源常量](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/runtime/constants.rs)、[生产取值](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/vmm/krun/engine.rs)、[release asset 元数据](https://api.github.com/repos/boxlite-ai/boxlite/releases/tags/v0.10.7)

<!-- /migrated-contract: SPEC:34.6 -->

<a id="spec-34-7"></a>
<!-- migrated-contract: SPEC:34.7 -->
## 34.7 条件验收与采用决定

以下细化 A29，仅在选择实现 BoxLite adapter 时执行，不把“研究一个候选”升级为必须采用：

| 场景 | 通过条件 |
| --- | --- |
| 持久与恢复 | 显式生命周期配置；stop/start、创建者退出、宿主重启分别核验；恢复等级真实，不丢已确认事件，不复活旧 lease |
| 分支与外部挂载 | clone 后各分支文件隔离符合声明；宿主共享目录不冒称已 COW；best checkpoint 提升不回退事实账本 |
| 本地与远端 GUI | 各自完成观察—操作—再观察；companion/tunnel 路径实测；分辨率变化和人工接管使旧输入失效 |
| 网络与自主升级 | image pull / guest / control+model 路径分别验证；Agent 自主决策；断链可恢复；读取实际发行能力 |
| 取消与递归预算 | guest process 终止或 unknown 可核验；C=1 时 Box 内孩子和视觉调用不绕过 broker；不按子 Agent 数盲目开 VM |
| 归档与版本 | 检查 secret、host path、平台与运行闭包；导入后重新绑定有效能力；主线、发行件、服务器能力一致性可追溯 |

采用判据是：它是否在目标设备上实质减少完整 Linux 环境、GUI 和文件分支的实现负担，同时满足本产品的持续单主体、插件化、隐私、真实恢复与全局推理约束。若验证成立，优先复用其现成机制；若某能力仍不足，保留独立 provider 和适配空间，不用改变用户原始需求来迁就后端。

<!-- /migrated-contract: SPEC:34.7 -->
