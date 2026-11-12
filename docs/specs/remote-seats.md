# Remote seats — functional contract

**内核定位。** 参考插件，登记在能力架上，建立在[内核合同](kernel.md)的[内核 5](kernel.md#kernel-5)、[内核 6](kernel.md#kernel-6)与[内核 7](kernel.md#kernel-7)之上。远端座位上结果未知的操作先对账，不盲目重发。

[§17](remote-seats.md#spec-17)、[§17.5](remote-seats.md#spec-17-5)、[§35.5](remote-seats.md#spec-35-5)、[§41.11](remote-seats.md#spec-41-11); carries PR127 (native SSH transport) into
installed task authority. Work package: `architecture-remote-seats-20261101`.

## What a seat is

A `TaskSeat` is a stable product identity for one remote-agent work position
(Store `seat_records`). A tmux pane id or an SSH target is only its current
*location*, never its identity. When work moves, the old record is retired
and a new record names it via `supersedes`; history is never rewritten.

## Installed native children

| kind | effect | durable facts |
| --- | --- | --- |
| `ssh.exec` v1 | one exact remote command over the PR127 transport (`native_transport::execute`) | frozen input, observation receipt (stdout/stderr/exit/interruption) |
| `ssh.pty` v1 | one live PTY seat: register seat, acquire v2 lease, attach adapter to `SeatRuntime` | seat record, v2 lease, attach receipt |
| `omp.seat` v1 | launch and attach one managed OMP RPC seat | seat record, v2 lease, attach receipt |
| `claude.seat` v1 | launch and attach one managed Claude Code control-protocol seat | seat record, v2 lease, attach receipt |
| `seat.observe` v1 | one observation of an attached seat | every reported question is a `seat_questions` row before the observation returns; `durable_only` status when no live seat is attached |
| `seat.respond` v1 | one answer to one open durable question | one `seat_answer_decisions` row per decision attempt, the exact effect digest, and the closing evidence |
| `seat.fleet` v1 | bounded read-only inventory, optionally filtered by task | original operation and CAS receipt of the durable records plus current transport observations |

All seven follow the product's at-most-once contract: freeze the input to CAS,
arm the exact operation id (`arm_external_operation`), effect once, observe
with evidence. Reconcile is query-only; a crash after T1 never re-sends a
command, re-opens a PTY or re-answers a question.

### Fleet observations

`seat.fleet` takes `{}` to discover the registered seats, or
`{"task_id":"<uuid>"}` to filter by owning task. It returns at most 32 newest
seats in deterministic order, with `truncated` when more records exist. The
query selects its bounded page before counting questions, and the runtime
probes only the returned seats, not the truncation witness. Each entry
contains its durable `seat_id`, owning `task_id`, adapter name/version, credential-free location
summary, status and open-question count. Retired and superseded records remain
visible; inventory does not silently drop history. Managed-process arguments
and session anchors are excluded from the location summary.

The runtime observes only transports already attached to it. `transport_live`
is `true` or `false` after a successful adapter check, and `null` when that check
fails or reaches its 500 ms deadline. An unattached record has
`transport_live: false` and `durable_only: true`; this says no local transport
is attached, not that a remote process is dead. `last_observed_at` is set only
after a successful current transport check. A durable record alone carries no
observation timestamp. `durable_only` is also true when a probe is unknown:
only `transport_live: true` contributes a confirmed live transport fact.

The inventory never attaches a transport, starts an agent, acquires a writer
lease, sends input or records a new question. Its native operation has
`EffectStatus::None`. Reconciliation returns the original settled receipt;
an unfinished read may take another snapshot using the original sealed task
selector, without creating transport effects.

The model discovery measurement lives in
`crates/shenko-exec-host/tests/seat_fleet_behavior.rs`. It runs the product
subject, broker, parser, native dispatcher and original CAS receipts in fresh
installations. Its natural task does not name the handler or provide a task
UUID. The observed Coder 390 run (`swift-qwen3.8-27b-nvfp4`) returned complete
inventories in 3/3 installed trials, compared with 0/3 when absent; all 3
unrelated-task trials avoided the fleet handler. These are bounded discovery,
inventory and committed-consumption observations. Final answer semantics,
remote control and long-term reliability are separate acceptance questions.
The [validation record](../evidence/seat-fleet-native-20261111.md) binds these
observations to their source, original receipts and retained evidence limits.
Set the explicit model endpoint/name and `SHENKO_USE_LOCAL_MODEL=1` to run the
measurement; ordinary offline runs skip it. Original Store/CAS and failed or
unstarted trials are retained beneath `SHENKO_SANDBOX_ROOT`.

## Writer lease v2 ([§17.1](remote-seats.md#spec-17-1) 单写者)

`SeatLeaseRequest { lease_id, seat_id, holder_task_id, lease_epoch (>0),
fence_capability }` commits `SeatLease { schema_version: 2, lease_epoch,
fence_capability, legacy_writer, ... }`. `check_seat_write` refuses stored v1
rows (`seat_lease_legacy`) and non-current or non-fence leases with typed
errors. Relocation retires the predecessor and ends its lease in the same
commit; the successor is a new record, so stale bindings break instead of
ageing.

The writer authority travels *with* the effect: `SeatWriteFence { lease_id,
lease_epoch, fence_capability, seat_id, seat_incarnation, task_id,
task_revision, question_id, question_revision, question_record_revision }` is
handed to the adapter that performs the write. The runtime does not
check-then-act: the durable claim and the arm happen before the transport
runs, and the epoch the decision was made under is the epoch the write
carries.

## The respond contract ([§41.11](remote-seats.md#spec-41-11))

`Store::commit_seat_answer_decision` is the **pre-effect claim**. It runs
before any input reaches the remote side and compares every version the
decider actually read:

| expectation | refusal when it moved |
| --- | --- |
| `question_revision` + `question_record_revision` | `stale_question` |
| `seat_incarnation` + `seat_revision` | `seat_location_changed` / `stale_task` |
| `task_revision` | `stale_task` |
| `lease_epoch` | `stale_authority` / `seat_lease_ended` |

On success the question becomes `decision_committed`, so a second decision for
the same question is refused (`seat_question_not_open`). The only exception is
a previous decision that ended `not_sent` — the remote provably never applied
it — so a *new* decision id may be made.

The runtime then runs the SPEC order, one durable fact per step:

1. **commit** the decision (`decision_committed`).
2. **plan** the exact bytes (`SeatEffectPlan { transport, payload, digest,
   summary, question_key }`). No mapping without evidence → nothing is
   written and the decision ends `not_sent`.
3. **preflight** the live state: the adapter re-reads the *current* protocol
   or screen state, never the observation the decision was made from. A
   refusal is typed (`question_gone` vs `state_changed`); a question that is
   gone is closed as `externally_resolved`, a changed state only refuses this
   write and returns the question to `open`.
4. **arm** the effect (`Store::arm_seat_answer_decision`): the digest and the
   carrying operation are recorded, the question becomes `responding`. From
   here the effect may have reached the remote side, so recovery never
   re-sends it.
5. **execute** the transport, outside every database transaction.
6. **record the evidence** (`Store::complete_seat_answer_decision`):

| observed | decision state | question state |
| --- | --- | --- |
| remote acknowledged | `answered` | `answered` |
| written, no acknowledgement | `answer_unknown` | `answer_unknown` |
| transport outcome unknown | `answer_unknown` | `answer_unknown` |
| refused before the write | `not_sent` | `open` (a new decision may be made) |
| remote rejected | `rejected` | `withdrawn` |

`answer_unknown` is the honest end state of an unconfirmed answer: the
question disappearing from the screen is not evidence that our answer took
effect, and an unknown effect is never replayed.

Question states are the SPEC set: `open`, `decision_committed`, `responding`,
`answer_unknown`, `answered`, `expired`, `withdrawn`, `superseded`, plus
`externally_resolved` and `late_answer_ignored` for a remote-side
resolution. A question whose wording changed, or a closed question that
appears again, gets a **new** durable question id — the old record is closed
as `superseded` and keeps its history.

## Adapters (crates/shenko-exec-ssh/src/seat/)

* `tmux` — existing tmux foreground TUI. Questions inferred from the styled
  screen; a selection is reported only from a preserved highlight attribute
  or a cursor glyph (`❯ › ➤ …`); numbered options map to their number;
  yes/no is recognized only on an observed `y/N` prompt and is never
  blind-typed. The guarded `send-keys` re-checks the pane on the far side:
  the pane must exist, must not be dead, must still run the observed program
  and must still host the observed root process (`pane_replaced`).
  A captured screen's final LF is a row terminator, not a live terminal
  newline: a full-height capture must retain its first row. Both `1.` and
  `1)` option markers retain their numeric input value; absence of a
  highlight or cursor marker leaves the selected option unknown.
* `ssh_pty` — managed remote agent over `native_transport::open_pty`. One
  single-owner pump feeds a `TerminalScreen` (full VT styles/grid,
  `terminal.rs`) and serializes writer commands, so writers never queue
  behind a held session lock. Same inference rules as tmux.
* `omp_rpc` — managed OMP (`oh-my-pi` v18 protocol): `ready`,
  `extension_ui_request` (select/confirm/input), `extension_ui_response`,
  `agent_end{isTerminal}`. A remote timeout is recorded
  `externally_resolved`, not an agent decision.
  Chunk assembly admits the first chunk's declared count and byte length,
  and requires the complete observed length to match before allocating the
  assembled payload; a parseable JSON prefix is not a complete message.
* `claude_sdk` — managed Claude Code control protocol
  (`--input-format stream-json --output-format stream-json
  --permission-prompt-tool-name stdio`; `initialize` before the first user
  message; `can_use_tool` → `AskUserQuestion` is an `information` question,
  any other tool is a `tool_permission`). `canUseTool` is not a uniform audit
  entry — calls an earlier rule approved never reach it — so task and effect
  ledgers remain this product's own records. An `AskUserQuestion` answer
  echoes the remote side's original `questions` array (kept verbatim in the
  question's `native_payload`) and keys its `answers` mapping by the
  sub-question the chosen option belongs to.

`SeatRuntime` (seat/runtime.rs) is the only live registry; `seat.fleet`,
`seat.observe` and `seat.respond` handlers share it. Fleet reads durable rows
from Store and probes attached transports. The other handlers perform question persistence, the
durable claim, the live preflight, the arm and the closing evidence.

## Native SSH transport additions (PR127 preserved)

`native_transport/pty.rs` adds `NativeSshSession::open_pty(request,
handshake)` → `NativePtySession` (live `NativePtyEvent` stream, send/resize/
signal/eof, `close(grace)` that counts post-close frames as `Protocol`
interruption). The single-command `execute()` path is unchanged.

### Explicit file-backed installation

`shenko start --ssh-targets <installation.json>` installs the `ssh.native`
task handler before native-child recovery. The
`shenko.ssh.file-installation.v1` document names explicit targets, their
selected private-key file, configured public key, known-hosts file and
remote receipt root. It does not discover credentials or select a different
node in place of the durable node-selection decision.

`shenko ssh-public-identity --installation <installation.json> --target <id>`
prints the configured public key and fingerprint with status
`configured_unverified`. It runs before Store/Tokio startup and does not open
the referenced key or trust file, authenticate, or claim possession of the
key. Execution uses the selected key through a bounded zeroizing buffer and
checks the configured public identity. This `ssh.native` path opens the key
before TCP; it is distinct from `ssh.exec`'s KEX-before-key path.

If the first reply is lost after the remote helper records its receipt,
recovery queries that original operation and input digest. It does not
upload again or create another attempt merely because local completion is
unknown. A wrong pinned host key must not reach authentication or the helper.

## Schema cutover (one explicit step)

`migrations.rs::migrate_seat_v2` is the single v1 → v2 cutover:

* `seat_answer_decisions` loses its `UNIQUE(question_id)` constraint — the
  uniqueness rule now lives in the admitting transaction, so a justified
  retry after a `not_sent` refusal is possible. The rows are copied, never
  dropped.
* stored v1 decision documents are brought to the v2 shape: the original
  `confirmation` string is kept and the state is derived from it.
* stored v1 leases are marked `legacy_writer` — they never carried a writer
  epoch or a verified fence capability, so they were never writers. The lease
  is not *ended* by the migration: ending a lease is a lease transition that
  belongs to a ledger commit (the first v2 acquisition displaces it).

## Boundaries

* Bindings are secret *handles*: the frozen input carries the `SshBinding`
  (key path + CAS-sealed known_hosts ref); the key file is read locally at
  effect time and never sealed into receipts.
* `use_agent` and password bindings are refused by the native children
  (agent-backed `Signer` impls can be admitted later without schema change).
* Remote `exit 255` is a real status; the legacy "local ssh process died"
  retyping does not apply.
* A written answer is transport, not acceptance: `AnswerConfirmation` stays
  `Unconfirmed` unless the remote acknowledged it.
* The writer lease is *this product's write authority boundary*, not a
  protection against human input. A human typing into the same pane, or
  another process, is not fenced by it — tmux has no atomic "check the
  question and type". That race is reduced (live re-check, target side
  pane/process guard) and reported, never hidden.
* Recovery queries only the original operation receipt and the durable
  decision facts; `unknown` never authorizes a replay.

<a id="spec-11"></a>
<!-- migrated-contract: SPEC:11 -->
# 11 SSH 身份与远端工具

Agent 可委派 keygen worker 创建身份；人类界面展示完整公钥、算法、指纹、创建时间、授权目标、轮换/撤销状态，并支持复制或下载。私钥属于 Agent 身份，不随 rootfs factory 回滚更换。

两种兼容模式均列入产品模型：托管私钥/签名能力；受保护、按 worker 挂载的密钥文件。建议优先托管，但当前 WASM SSH port 是否支持所需签名桥必须实证；若只能使用文件，明确标为兼容模式，最小范围注入、用后清理，不将其混入普通 snapshots。不能声称现有 port 自动支持 host ssh-agent。

首次连接验证 host fingerprint/host CA/已登记节点身份；可配置 TOFU 供无人值守使用，不能把未经验证的首次主机描述为已证明可信。known_hosts 变化产生独立可见状态，不因为普通网络重试静默接受新身份。

统一工具面向小模型提供：stat/list/read/search、带版本或内容 hash 的 patch/write_atomic、upload/download，以及 argv 化 exec、stdin、stream、poll、cancel。资源 URI 可采用 ssh://target/path；目标选择和凭据绑定由已验证 target ID 完成，不让模型每次拼用户名、端口和私钥文件路径。

超时、输出截断、远端退出码、连接断开与执行结果未知必须不同。断开不能证明远端命令停止。高影响命令可采用远端 operation ID/receipt helper；不支持时由 worker 验证状态再决定重试。连接复用、SFTP、跳板/多跳等按实际 port 能力曝光，不照抄原生 oh-my-pi ControlMaster 就声称 WASM 已支持。

未来完整容器与自有节点实现同一 ExecutionProvider/ResourceProvider；节点报告架构、环境、能力和健康。主要连接方式是用户已授权给 Shenko 使用的 SSH Key；在既有授权内自主连接和操作，不为每个已授予能力再设人工 gate。节点管理协议可以增量补充，不强迫所有远端安装完整主 Agent。

U57 的内网直连/直接感知发现、Tailnet 与 ZeroTier 是可组合的发现和可达路径。相应感知插件可以自主发现已安排设备的地址、健康和能力，并将现场观察交给主体；已登记节点身份与授权仍按现有 SSH/节点合同核对。发现地址或读到外部设备描述不自动产生操作权限，路径变化也不改变同一设备/自我的身份。

Shenko 可通过有限执行 child 使用任意已授权设备真实提供的能力：root Android 的完整屏幕控制与任意 shell，Linux、macOS、Windows 的相应屏幕、应用、文件、shell 及可用管理能力，均是正式扩展目标。设备能力随实际账号、root/系统权限、会话与 provider 绑定呈现，不将普通权限冒称 root，也不把远端节点一律缩成文件服务器。同步自我与操作设备共用 [§5.4](nodes.md#spec-5-4)、[§22](nodes.md#spec-22)、[§24](desktop-runtime.md#spec-24) 的身份、状态和效果合同；凭据按现有凭据管理器绑定，不因同步记忆而复制私钥到普通知识正文。

<!-- /migrated-contract: SPEC:11 -->

<a id="spec-17"></a>
<!-- migrated-contract: SPEC:17 -->
# 17 远程 Agent 遥控插件

首版 SeatObservation、问题 revision、输入 lease、respond 的前置条件、确认及 unknown 恢复见 [§41](architecture.md#spec-41)；以下能力目标不得被具体适配降格为只看文本或盲发按键。

<!-- /migrated-contract: SPEC:17 -->

<a id="spec-17-1"></a>
<!-- migrated-contract: SPEC:17.1 -->
## 17.1 任务与 seat 的连续性

RemoteAgentProvider 是可安装原生插件，统一支持已存在的 Claude Code、omp、其他 agent seat 和 tmux pane。来信进入主体后，主体通过语义 memory 找到任务对应 runtime/node、仓库/cwd、seat/session、最近 checkpoint、接管方式与已授能力，再委派远程操作子 Agent。记忆中的位置是线索，执行前必须核验现场是否仍是该任务。

TaskSeat 记录稳定 seat ID、adapter 类型/版本、host identity、远端 agent/session ID、tmux server/session/window/pane ID、进程标识、cwd、task lineage、会话来源、最近观察版本和 owner/lease。任务可搬迁，历史定位保留 supersedes 关系；不能把过时的 pane 编号当永久身份。

插件提供 discover/inspect/attach/submit/steer/interrupt/read_events/respond/resume/detach 与问答/权限事件。优先使用结构化 SDK/RPC/事件接口；只有现成交互终端无此能力时使用 tmux capture/control/send-input 路径。OMP 已公开 stdio JSONL RPC，Claude Agent SDK 有工具许可和用户问答回调；具体版本能力需要 adapter 契约测试。[OMP RPC](https://github.com/can1357/oh-my-pi/blob/main/docs/rpc.md)、[Claude 用户输入](https://code.claude.com/docs/en/agent-sdk/user-input)、[Claude 权限](https://code.claude.com/docs/en/agent-sdk/permissions)、[tmux 手册](https://man7.org/linux/man-pages/man1/tmux.1.html)

远程执行路径仍属于子 Agent，主体不直接 SSH 或向终端输入。一个 seat 同时只有一个有效写入操作者；人类接管或另一个 worker 取得新 lease 后，旧写入被拒。终端输入前核验程序/会话及提示版本，防止 Agent 已退出到 shell 后把选择答案当 shell 命令执行。断线后先读现场再重发，不盲目按 Enter。

<!-- /migrated-contract: SPEC:17.1 -->

<a id="spec-17-2"></a>
<!-- migrated-contract: SPEC:17.2 -->
## 17.2 自主处理问题与许可

远端 AskUserQuestion、选项框和 permission prompt 统一为 DecisionRequest：问题 ID、seat epoch、任务关联、选项及含义、原始 action/参数、超时与可恢复方式、授权主体、最新状态。问题是规划选择、补充信息、工具许可还是确实缺少新授权，要能区分；不能把所有弹窗统一映射成“按同意”。

**已委托范围内，Agent 有权自主决定选项并继续任务。** 它可以根据目标、用户偏好、长期授权和现场证据选择，给出简洁决定理由与回执，无需每个远端 permission 都打扰用户。用户可以授予很宽的持续权限；插件不额外发明固定逐项批准流程。

决定者是当前有效主体，或主体授予明确决策范围的有限子 Agent；具体按钮/API 响应由远程操作 worker 执行。超出既有授权或确实拿不准时，主体选择补查信息、试验、等待、寻找可联系的人或改变方案。由 Agent 形成联系计划，不采用“未知一律硬报错”和固定唯一通知渠道。

RemoteAgentProvider 必须拒绝过期 question 的回答，记录人类与 Agent 哪一方先完成决定；timeout/default/recommended 不能冒充 Agent 已作出决定。对无法暂停很久的远端客户端，使用其已支持的 defer/checkpoint/resume，或记录阻塞现场再重建；不能假定每个 CLI 都支持永久等待。

<!-- /migrated-contract: SPEC:17.2 -->

<a id="spec-17-3"></a>
<!-- migrated-contract: SPEC:17.3 -->
## 17.3 主动寻找人类

Contact/Message provider 向主体提供已授权联系路径、可达性、近期送达、时区/安静时间、敏感内容可发送范围和回复关联。主体根据紧急程度、人格、用户习惯与过去反馈选择先联系哪里、如何措辞、多久再试，必要时并行/轮换多个渠道。可使用所有已连接且获准的联系办法，不依赖单个硬编码 fallback。

每次询问具有跨渠道 decision ID，任一渠道的有效答复使其他待询问副本收敛；不同人冲突回答回到同一主体处理。敏感任务可先发简短提醒和受控链接，不能因“所有方式联系”把秘密全文散播到公开群。投递失败是下一轮思考的观测，不能替代 Agent 自己决定接下来怎么办。

所有都联系不上时，主体继续其他工作并保留这项待决任务，按其策略再尝试或采用已获授权的保守替代；不能声称沉默就是新授权。

<!-- /migrated-contract: SPEC:17.3 -->

<a id="spec-17-4"></a>
<!-- migrated-contract: SPEC:17.4 -->
## 17.4 远端推理资源

控制一个远端 Agent 不等于控制其每次模型请求。若它也使用本地共享 API，必须经统一 broker/proxy，子 Agent 再开子 Agent 也不能绕限；若无法接管它的推理入口，则标为 unmanaged。只有可验证且可强制的限额已从共享总额扣除，才可用容量预留维持相应保证；未知外部并发不能靠填写一个预留数解决，必须明确未覆盖、不能宣称物理全局硬上限仍完整成立。远端自有 GPU/云后端可作为另一个资源组，仍记录成本与任务 lineage。

<!-- /migrated-contract: SPEC:17.4 -->

<a id="spec-17-5"></a>
<!-- migrated-contract: SPEC:17.5 -->
## 17.5 终端观察与语义整理

PTY 路径先用终端状态机处理 ANSI、重绘、窗口大小与光标，输出带 revision 的 screen grid/text runs，再由 adapter 提取有限 diff、候选问题、选项和选择状态。观察应保留需要的前景/背景/反显等属性、尺寸、cursor、pane/process/seat identity、捕获时间及原始证据引用；光标位置不自动等于 TUI 选中项。[vt100 crate](https://docs.rs/vt100/latest/vt100/)

tmux 原生 `capture-pane -e` 可以保留文本与背景属性的 escape sequences；默认纯文本 capture 不具备同样信息。缺高亮时先验证该参数、正确 screen buffer、SSH/编码链路和 parser 是否保留属性，再决定写 wrapper、终端语义插件或外部应用。Control mode 可提供持续 pane 输出，但其转义仍需正确解码。不能从“所有选项文字都在”推断当前选中了哪一项。[官方 manual](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/tmux.1)、[capture 实现](https://github.com/tmux/tmux/blob/1c418f0b62d131b57d2e113362c2911867102652/cmd-capture-pane.c)

模型通常接收结构化观察与必要的选中候选，不把 spinner/完整 ANSI 历史灌满 context；未知或丢失的选择状态明确报告为能力缺口。先有 Claude/OMP 的结构化 RPC/SDK 问题事件时使用相应接口，终端解析保留为实际可用的兼容路径。Unicode/宽字符、反显、重复选项文本、alternate screen、resize、断线和问题消失均需验收；输入前重新验证有效 seat/question/lease。完整自主补全流程见 [§18.7](cognition-units.md#spec-18-7)。

终端 parser 的画面 diff 不是已验证的问题语义；静默、历史中的 y/N、看起来像 AskUserQuestion 都不能证明当前程序正在等待回答。优先使用 SDK/RPC 事件；终端推断绑定 seat/process/question revision，输入前重读现场，核验目标仍消费该问题。问题已被人类处理或程序退出时，旧答案不能被送进 shell。采纳语义整理以节约上下文，不把启发式解析伪装成可靠的权限协议。

<!-- /migrated-contract: SPEC:17.5 -->

<a id="spec-35-5"></a>
<!-- migrated-contract: SPEC:35.5 -->
## 35.5 三条 Seat 路径，各自兑现能力

| 路径 | 首版实现与边界 |
| --- | --- |
| 已存在的 tmux 前台 TUI | 必须支持，不能只交付重新启动一个 headless agent。先核对 pane、进程、cwd、session、任务与观察 revision；读取保留属性的屏幕，识别真实高亮，再输入。Claude 与 OMP 作为首批实测目标；其他程序报告 generic observation／专用理解的真实级别 |
| 托管 OMP | 优先使用当前实际支持的 JSONL RPC／rpc-ui，接收结构化问题与事件。用可替换 companion 或等价服务持有长连接和日志；控制器短暂重启不关闭唯一 stdio 所有者 |
| 托管 Claude Code | 使用实际版本支持的 SDK、stream-json、session resume 或原生 background session。按能力探测选用；原生后台 attach、恢复历史 session、控制任意既有 tmux TUI 是不同能力 |

OMP 当前 RPC 区分 prompt 接收、prompt_result 与 session_settled；一轮结束后仍可能有后台工作。rpc-ui 的工具问答和 extension approval 也不同：--no-ui 仍可保留某些工具 UI，但会使需要 extension approval 的工具缺少相应交互路径，适配器必须逐项协商。当前 Claude SDK 能经 canUseTool 承接问题与权限；defer 有非交互模式及单工具调用等条件。不得把这些能力概括成任意 TUI 都可 SDK 接管。[OMP RPC](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md)、[Claude 输入](https://code.claude.com/docs/en/agent-sdk/user-input)、[Claude hooks](https://code.claude.com/docs/en/hooks)、[Claude CLI](https://code.claude.com/docs/en/cli-reference)

tmux capture-pane -e 可保留文本/背景属性；control mode 提供经过转义的应用输出，需还原后重建屏幕，tmux 自己的 copy/choose mode 另行观察。文字、前景、背景、reverse、宽字符坐标、cursor、选中范围、尺寸和 observation revision 都是有意义的状态。画面静默只帮助读取，不证明当前可安全选默认项。[tmux 手册](https://man.openbsd.org/tmux)、[Control Mode](https://github.com/tmux/tmux/wiki/Control-Mode)

Seat writer lease 在所有写入经过受管通道时才可强制执行。未经该通道的本地人类输入或其他进程不能被虚构成已经围栏；人类接管使旧决定失效，原生 tmux 无原子“校验问题并输入”能力时必须公开剩余竞态，并通过现场复核、协作接管或专用协议降低它。不会为一次观察陈旧的问题盲目重发按键。

<!-- /migrated-contract: SPEC:35.5 -->

<a id="spec-41-11"></a>
<!-- migrated-contract: SPEC:41.11 -->
## 41.11 远端 Seat 问题、决定与答复

### Seat 与问题记录

~~~text
TaskSeatBinding {
  seat_id, seat_revision, seat_incarnation,
  adapter_id, adapter_version, host_identity_ref,
  remote_agent_session_id?, tmux_server_id?, session_id?, window_id?, pane_id?,
  process_identity: { pid?, process_start_token?, executable_digest? },
  cwd_identity_ref?, bound_task_id, bound_task_revision,
  writer: { lease_id, lease_epoch, holder, fence_capability, expires_at? },
  observation_ref, observation_revision, event_cursor_ref?,
  supersedes_binding_ref?, record_revision
}

SeatQuestion {
  question_id, question_revision,
  seat_id, seat_incarnation, task_id, task_revision,
  source_kind: native_rpc | sdk_callback | terminal_inference,
  native_request_id?, native_tool_use_id?, origin_event_ref,
  question_kind: planning | information | tool_permission | new_authority,
  text_ref, options: [{ option_id, native_value, label, meaning_ref? }],
  requested_action_ref?, exact_input_digest?, answer_schema_ref,
  observed_screen_revision?, raw_evidence_refs[],
  expires_at?, external_default_ref?,
  state: open | decision_committed | responding | answer_unknown |
         answered | expired | withdrawn | superseded,
  effective_answer_ref?, closure_evidence_ref?, record_revision
}

AnswerDecision {
  decision_id, decision_revision,
  question_id, expected_question_revision,
  expected_seat_incarnation, expected_task_revision,
  deciding_actor, delegation_grant_ref, authority_revision,
  selected_answer, exact_effect_input_digest,
  reason_summary, evidence_refs[],
  operation_id, valid_until?, committed_at: CommitRef
}
~~~

question_revision 只在问题语义、选项、实际 action/参数、期限或相关绑定变化时增加；spinner、无关日志、光标闪烁等变化增加 observation_revision，不无意义地产生新语义问题。一个问题关闭后又出现，即使文本完全相同，仍有新的 question_id 或原生请求身份。不能用整屏文字 hash 代替问题身份。

terminal_inference 必须保留置信与未知信息；缺少高亮时不得根据选项顺序猜当前选择。tmux capture-pane -e、screen buffer、ANSI/宽字符等恢复由观察插件负责。独立 wrapper 能力提升经过安装/验证后，新的观察合同和版本写进后续 Question，不能改写旧证据。

### respond 合同

respond 接受 question_id、expected_question_revision、expected_seat_incarnation、expected_task_revision、decision_id、expected_writer_lease_epoch 和稳定 operation_id。模型只决定语义答案；确定性 adapter 把它映射为准确的 native response 或有限终端输入，保存最终 wire/input digest 后才 arm。

| 步骤 | 必须验证/提交 | 失败或崩溃后的处理 |
| --- | --- | --- |
| 选择答案 | 主体或有明确 decision scope 的有限 child；既有委托覆盖实际问题/action | 常规实现选择、已授权限内许可可自主决定；不额外固定问人。确需更多信息/新授权时形成后续调查或联系计划 |
| 提交 AnswerDecision | 问题仍 open；语义 revision、seat/task/authority 版本匹配；答案符合 schema | CAS 失败返回 stale_question/stale_task/stale_authority；保留候选但不输入 |
| preflight | 通过当前受管 writer 获取最新现场/原生请求存活状态，确认仍是原程序和问题 | 人类已回答、进程退回 shell、pane 复用、任务变更即使旧屏幕仍像问题也不继续输入 |
| T1 arm | 再核对当前 Decision/lease/revision；登记 operation 与准确 effect input | armed 后即存在“可能已送出”窗口；不可用临时进程内 sent 标志作唯一依据 |
| 实际输入 | 在事务外使用原生响应接口，或受管终端写入通道 | 网络/SSH 写完成最多证明 transport_written，不证明远端接受答案或其工具已经成功 |
| E1/T2 回执 | 归档原生答复确认、问题结果、工具/任务后继或其他明确证据；记录真正生效的答案来源 | 无确认则 answer_unknown；问题从屏幕消失本身不能证明是我们的答案生效 |
| 结束/恢复 | answered/expired/withdrawn/superseded 均保留 closure evidence；恢复先 inspect 原 request/seat | 不重发未知的 Enter 或 UI response。安全重试只在 adapter 明确提供幂等/去重合同或确认未应用时成立 |

“已作决定”和“答案已生效”分开。候选答案可在发送前被较新事实取代；T1 后禁止原地改同一 operation 的 input，改变主意产生新的决定/operation，并先确定旧效果。人类与 Agent 的竞态以目标端真实处理证据或受管通道的原子仲裁确定；不能凭两边本地时钟先后宣布获胜。

原生请求 ID 和有效 callback 可提供比画面更强的关联，但不自动具有跨进程 durable 去重。原始 tmux 没有原子“比较屏幕/问题 revision 并输入”的操作：受管 writer 可串行化本产品的输入，无法防止旁路人类输入。该路径明确标 check_and_input=best_effort；每次输入前现场复核并监听接管，不能把本地 lease 宣传成目标端强围栏，也不因此给所有操作新增人类批准步骤。

AnswerDecision 允许 allow、deny、选项、文本、修改后的参数等适合该原生请求的结果；扩大成永久权限规则是独立语义，不能因为一次 allow 顺便提升长期授权。QQ 发信人的平台身份、联系人推断、紧急级别与管理授权分别验证；开放聊天不把群员或群主提升为管理员。

### 真实上游语义，不将展示或默认选择当成 Agent 决定

- [OMP 固定 RPC UI 协议](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md#L1202) 提供带 id 的 UI request/response，并区分 tool UI 与 extension approval。超时/abort 可产生 cancel/targetId，迟到答案会被忽略；ask 的超时默认可能选 recommended 或第一项。Shenko 应记录 external_default，并继续核验后果，不能记成“主体自主批准”。

- 同一 OMP 文档的 [yield/settled 说明](https://github.com/can1357/oh-my-pi/blob/0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0/docs/rpc.md#L345) 区分 prompt_result 与 session_settled。前者不证明后台工作全部结束；也不证明某次 UI response 已取得外部 exactly-once 保证。

- [Claude 官方 user-input](https://code.claude.com/docs/en/agent-sdk/user-input) 将工具许可与 AskUserQuestion 交给 canUseTool；callback 的取消信号与实际请求一起绑定。该接口可以由 Shenko 的受委派决定者回答，不要求产品再固定插一轮人类审批。callback 的内存 Promise 不能替代跨进程问题记录；defer/resume 要使用实际版本支持的条件。

- [Claude 官方 permissions](https://code.claude.com/docs/en/agent-sdk/permissions) 的 canUseTool 不是所有工具的统一审计入口；自动允许的调用可能先被解决。因此任务 scope、效果记录与全局推理约束不能只放在一个可能不被调用的许可 callback 中。

### 动态联系与旧答复

确需联系人的问题登记 ContactPlan：decision_id、question/ref 与 revision、目标 person/已验证账号关系、候选已授权渠道、内容可见范围、已发 operation、答复关联、复核条件。主体按人格、记忆、可达性与急迫程度选择行动；不用固定三分钟/固定电话作为内核规则。

跨渠道答复先作为普通持久外部事件进入同一主体，再绑定其引用的 decision/question revision。一个有效答复使其他未发送或仍待答副本收敛；旧 question 已关闭时，答复保留为后续上下文，不自动落到新问题或 shell。没有人答复不是新授权；主体可继续其他工作、自行补查或采用已经获授的替代方案。

<!-- /migrated-contract: SPEC:41.11 -->
