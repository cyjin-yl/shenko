# Web 界面契约 — SPEC §21 / §41.18 / §43（C11 + architecture-web 切片）

本文件是 `crates/shenko-ui` 的对外契约：WS 协议、持久输入、撤回、富呈现、
流式 part、能力协商，以及浏览器客户端的诚实边界。原始 SPEC 前缀保持不变；
实现以 ledger 事实为准，传输成功永不冒充主体接纳。

原生默认入口为 `http://127.0.0.1:<port>`，不自动监听外网。远程部署应提供
明确的认证访问路径。无模型首启与有模型运行装配同一 owner claim 服务；
安装控制凭据不通过页面访问先后、启动日志或模型上下文交付，见
[安装认领与配置契约](configuration-playground.md#ownership--444)。

## 1. WS 协议（`/ws`，全部 JSON，`schema_version` 逐消息校验）

### 1.1 服务器 → 客户端

| type | 语义 |
|---|---|
| `hello` (v2) | `server_version` + `capabilities`（append/steer/withdraw/actions/documents/attachments/durable_input_receipts/committed_replay/streaming）+ `surface`（基线能力事实，revision 0） |
| `capabilities` (v2) | `client_hello` 的应答：`surface` 重算（revision 递增，含 `valid_until`） |
| `timeline_part` (v2) | 已提交 part：全量正文，`chunk_seq=0`，`terminal=true`，`committed_only=true`；`stream/request/attempt` 缺省（当前 OutboxMessage 无此绑定，不推断）。携带流式前缀的 part 会额外发出修正 part（`part_id="text/0@<commit>"`），客户端按替换处理 |
| `stream_part` (v2) | 流式 public-text chunk（仅当真实生产者接入；`streaming:false` 时永不发送）。`step` 可为 0（尚无已提交步；客户端不得虚构）；其余 identity 与 `timeline_part` 同构 |
| `document` (v1) | 已校验并按本连接能力降级的 SemanticDocument（见 §3） |
| `document_error` (v1) | 文档块无法成为界面；原文仍可见，富层丢弃，原因可读 |
| `identity` | 主体身份 revision 变化通知（既有） |
| `receipt` (v2) / `submission_view` (v2) | 见 §2 |

### 1.2 客户端 → 服务器

| type | 语义 |
|---|---|
| `client_hello` (v2) | 显示事实 attestation：`viewport`、`language`、`theme`、`font_scale?`、`reduced_motion?`、`low_bandwidth?`。仅影响降级，不产生权限 |
| `submit` (v2) | `submission_id`/`message_id`/`subject_id`/`mode: append\|steer`/`content`/`attachment_refs`/`observed_step_id?`。拒绝未知字段 |
| `query_submission` (v2) | 重连对账：`submission_id`+`subject_id`+`send_epoch?`（客户端计数，仅回显） |
| `withdraw` (v2) | `submission_id`+`subject_id`+`cancel_id`；持久控制记录，不删除已接纳事实 |
| `action` (v2) | `document_id`/`action_id`/`revision`/`nonce`/`payload`（取自 DocumentOption 的 canonical payload）；服务端重算 blake3，不信任客户端摘要 |

## 2. 持久输入与撤回（§41.18）

- 接纳顺序：先 `Store::admit_web_submission`（InboxEvent + submission 绑定同事务），
  再返回 `receipt`。同一 `(subject, submission_id)` 同 payload 重放返回原记录
  （`replayed:true`）；不同 payload 为 `conflict`，原始事实永不改写。
- 传输断开≠丢失：客户端以原 `submission_id` 查询/重发；服务端从 ledger 对账。
- **placement 诚实**：当前 adapter 无已验证的请求内控制能力，append 与 steer
  一律 `actual=next_boundary`（`target_request_id` 在已封口 steer 时记录）。
  `live_steer`/`cancel_reseal` 仅存在于线协议，本代不产生。
- **撤回状态机**（store 侧归 ArchSubject）：未呈现（未 disposition、未封口选入、
  未 stage 进 Planned 步）→ `withdrawn_before_presentation`（S0 消费时排除）；
  否则 `withdrawal_queued`，由 S-折入注解。行永不删除；`commit_seq` 提供全序；
  重启后按 ledger 重放。原始事件与接纳事实保留。
- `submission_view.result`：`found`/`not_admitted`（持久证据）/
  `temporarily_unavailable`（永不冒充 not_admitted）/`invalid`。

## 3. 富呈现（§21.5–§21.9）

- 已提交正文中的 ```` ```shenko-ui ```` 围栏块为 `SemanticDocument`（schema v1）。
  校验：已知节点 kind、action 绑定（payload blake3 = canonical JSON 摘要）、
  fallback.summary 非空。校验失败 → 文本降级，绝不出半可执行界面。
- 降级阶梯：`rich → card → text`；每档保留编号 ActionOption，option N 恒定对应
  同一 `(action_id, nonce)`，“选 2”与按钮点击是同一决定。
- 能力协商：`SurfaceCapabilities{revision, valid_until, streaming(服务端事实),
  native_cards, buttons_forms, …}`；客户端事实仅 attestation，`streaming` 只来自
  真实接线。未知能力降级为已确认基础形态。
- 动作执行权：点击只接纳一条持久输入（dedup key = submission 键，`kind="ui_action"`），
  主体像处置任何输入一样处置它；`actor_scope` 非 owner 拒绝。

## 4. 流式 part（§21.11）

- 表 `subject_output_parts`（PK `(agent_id, stream_id, message_id, part_id, chunk_seq)`，
  索引 `(agent_id, commit_seq)`）。Append-only：同键同内容=幂等重放（返回原 commit），
  同键异内容=conflict（不改写）。无 update/delete 路径。
- `content_kind` 恒为 `public_text`：脱敏是生产者责任，本表不再过滤。
- **生产者契约（依赖 ArchBroker/ArchSubject）**：`message_id`=arm 时预分配的
  step_id；`part_id="text/0"`；`part_revision`=attempt 级 revision；chunk_seq 从 0
  连续；producer 在写入前保证 public-text 脱敏。`StreamSink::publish` 先持久后广播；
  `parts_after(cursor)` 供重连断点续传；chunk_seq 空洞由读取方 surfaced，绝不补。
- UI 诚实：无生产者时 `streaming:false`，无 chunk 路径，无打字机动画；
  已提交 replay 不受影响的。

## 5. 客户端（web/src）

- `projection.ts`：UiState 增 `capabilities`/`surface`/`streaming`/`documents`/
  `document_errors`；Submission 增 `placement`/`withdrawal`；新增
  `ServerCapabilities`/`SubmissionPlacement`/`WithdrawalView`/`DegradedDocument`/
  `ActionOption` 类型；composer 增 steer 动作；submission 节点增 withdraw 动作
  （按 props 传 action id，不靠 refs 下标）。
- `live-controller.ts`：hello 解析全部能力；onopen 发 `client_hello`；新处理
  `capabilities`/`document`/`document_error`/`stream_part`；`applyChunk` 统一
  committed/stream 合并——committed 全量正文 seq 0 被吸收为 S1 绑定（两种到达
  顺序幂等），流 chunk 落在 committed 基 part 只补 identity 不补文本；空洞进
  pending buffer，绝不拼接；`sendWithMode`/`sendWithdraw`/`sendAction`。
- `react-renderer.tsx`：SemanticDocumentView（summary + 编号选项按钮，Rich 显示
  节点树 JSON）、ReceiptView 增 placement/撤回按钮/撤回标记、Composer 增
  Steer 按钮与流式事实文案、day 分组兼容 document 节点。详情页按投影的
  `kind` 读取 `tasks_surface`、`memory_surface`、`config_surface` 和
  `playground_surface`；导航后呈现接口的实际数据、空状态或错误，不能只改变
  当前导航项而留下空白页面。
- 已提交消息与流消息共享同一条消息校验和类型边界；只有校验后的消息可进入
  拼接与待补读缓冲区。流消息允许 `step=0`，已提交消息仍要求正数步骤与
  `step_id`；类型检查不能靠转换、占位步骤或放宽消息校验通过。
- `demo-controller.ts`：补齐新 UiState 字段（demo 全 false/空）。

## 6. Proto-UI pin

`web/upstream-pin.json`：`971cd35a4604ef19ecd3b6ffe3cbd01cf3e6633e`
（migrated_from `7181a93…`）。上游 `pnpm-lock.yaml` sha256 不变
（`3ac437fd…`）；adapter-react 签名与 shadcn 导出经比对一致。

构建时由该提交的 `scripts/build/public-packages.mjs` 解析 pin 中
`adapter-react`、`prototypes-shadcn`、`cli` 三个入口的依赖闭包，而不是调用
另一份源码或使用旧提交的 `dist`。本 pin 的闭包为 42 个包；这不代表全部
上游适配器均已构建或运行。`PROTO_UI_SOURCE` 必须通过真实 Git HEAD 和 lock
摘要校验，`PROTO_UI_BUILD_RECEIPT` 绑定这次上游构建。使用 Node 24 与锁定的
依赖运行类型检查和 `web/build.mjs`，由真实 adapter、esbuild 和官方 CLI
生成资源、样式、许可及带资源摘要的 manifest。Rust 将这些资源嵌入二进制，
因此资源重建之后还必须重新编译并启动 UI；修改 manifest 或只打开旧二进制
不能证明升级生效。

### compiler：构建期编译 Shenko 自己的 Prototype

该 pin 的 `@proto.ui/compiler` 是私有源码包：`exports` 只有
`./src/index.ts`，没有 `dist`，也不在 42 个公开包的构建选择里。所以它既不能
当包产物导入，也不能假装存在运行期编译器。现行路径是**构建期编译**
（`web/compiler.mjs`）：

- 用同一份审查过的源码，审计 `packages/compiler/package.json` 的包身份、私有标记
  与 `exports` 入口，再对其 `src/` 计算确定性目录摘要；
- 用工作区锁定的 esbuild 把该 TypeScript 模块图打成一份 Node ESM
  （`generated/compiler/`），`typescript` 保持外部依赖（compiler 声明 `^5.0.0`，
  由 web 锁定的 devDependency 满足）；`@floating-ui/dom` 由声明它的
  `packages/modules/positioning` 安装副本解析并记录版本与摘要；
- 用 `compileFile` 编译 `web/prototypes/*.proto.ts`（Shenko 自己写的
  Prototype），profile 为 `react-dom-source-v1`，输出 create-only 写入
  `web/generated/prototypes/<name>/`，含 compiler 自己的 `provenance.json`；
- 浏览器 bundle 只导入生成的 React DOM 源码，因此 manifest 记
  `runtime_imported: false`；compiler 源码摘要、bundle 摘要、每个 Prototype
  的源字节摘要与生成摘要都写进 `build-manifest.json` 的 `compiler` 事实里。

`web/generated/` 是构建产物、不入库；`tsc` 需要先有这次编译，因此
`pnpm check` = `node build.mjs --prototypes-only && tsc --noEmit`。

界面上的 transport 状态徽标就是这个编译产物：Prototype 只负责呈现（props
`label`/`tone` 与 token 样式），连接事实、文案与推理仍留在
`react-renderer.tsx`，没有搬进 compiler 或 Prototype。

界面验收需实际切换页面并对照当前接口与嵌入 manifest；壳层可见、HTTP 200
或构建成功均不足以证明页面可用。独立 UiServer 的只读页面验证不替代真实
主体、provider、持久输入和流式接线验收。

## 7. 依赖与集成注记

- `Store::submit_web_withdrawal`/`web_input_withdrawal` 与 raw_session.rs 撤回排除
  归 **ArchSubject**（其树已落地；本树调用点已就位）。
- `InferenceStreamEvent` 生产者与 part 写入器归 **ArchBroker/ArchSubject**；
  未接线时 `streaming:false`，全部测试不依赖流。
- server.rs 中的 `media_runtime` 字段/`with_media_runtime()` 归 **ArchMedia**。
- 父代执行：cargo check/test（shenko-store + shenko-ui）、
  `node build.mjs --prototypes-only` + `tsc --noEmit`、`node build.mjs`、
  原生浏览器验证、发布前 pin 新鲜度。构建产物（`src/static/*`）与 Rust 嵌入
  由父代重建；未重建时不得声称界面已升级。
