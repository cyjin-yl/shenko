# 浏览器能力功能合同（docs/specs/browser.md）

**内核定位。** 参考插件，登记在能力架上，建立在[内核合同](kernel.md)的[内核 5](kernel.md#kernel-5)、[内核 6](kernel.md#kernel-6)与[内核 7](kernel.md#kernel-7)之上。浏览器引擎、会话与接管方式都可以替换。

绑定：[§24.6](browser.md#spec-24-6)（原生浏览器能力、持久身份与人类接管）、[§19.1](repl.md#spec-19-1)（多 kernel 与统一会话生命周期）、[§41](architecture.md#spec-41)（插件效果声明、重试/取消/补偿与恢复边界）、[§24.3](desktop-runtime.md#spec-24-3)（显示域接管语义）。实现位于 `crates/shenko-exec-host/src/browser/`，共享运行装配位于 `bin/shenko/src/lib.rs`。

本文件定义浏览器能力的功能、接口与恢复合同。身份/会话/接管/恢复语义以 [§24.6](browser.md#spec-24-6) 为准；源码存在或编译通过不替代真实引擎运行证据。

## 1 能力定位

浏览器是原生可发现、可解释的受委派能力：worker 通过类型化原生任务（native task kind）打开/复用标签页、导航、观察、交互并取得结构化结果；本包不新增第二条 agent 循环，不内置自主策略。`shenko browser` CLI 子命令是同一 `BrowserProvider` 的直接驱动面，仅用于原生验证与诊断。

引擎发现只用节点上已安装、路径被钉住的浏览器（`chromium`/`chrome`、`firefox`/`camoufox` 的系统与 PATH 候选，加 `SHENKO_BROWSER_ENGINE` 覆盖），并实测 `--version`。从不自动下载浏览器；无可用引擎时打开会话返回明确错误并报告探测过的位置。默认虚拟显示（Xvfb）+ 有头引擎 = virtual-headed；`SHENKO_BROWSER_DISPLAY` 可指向人类显示域；`mode: headless` 显式真无头。节点无任何显示能力时，`auto` 模式如实降级为 headless 并在 `mode_degradation` 记录原因。

## 2 协议适配层

`protocol.rs` 定义引擎无关 `BrowserProtocol` trait（目标/标签页、导航、JS 求值、点击/输入/滚轮、cookie/storageState、截图、AX、关闭），两种真实适配：

- `protocol_cdp.rs` — Chromium DevTools Protocol（`Target.createTarget` + flatten session、`Page`/`Runtime`/`Input`/`Storage`/`Accessibility`），endpoint 从 DevToolsActivePort 文件读取，端口 0 由浏览器自选，不猜测。
- `protocol_bidi.rs` — WebDriver BiDi（Firefox，`session.new` + browsingContext/script/storage/input 模块），ws URL 从引擎 stderr 解析。

两者共用 `WsClient`：请求/响应按 id 配对、事件进广播 channel、协议级错误文本进 `error_text`（导航失败、JS 异常都不静默为成功）。HTTP helper 支持 chunked 解码用于 CDP 发现。不支持的观察维度如实上报（如 BiDi 的 `ax_available=false, ax_note`），不伪造。

会话枚举保留原生查询错误：某个会话的事实读取失败时返回错误，不把该会话静默删出结果并报告成功。Firefox 发布首个 BiDi 地址后继续排空 stderr；一次性地址通知不重复消费 sender。

## 3 身份与会话（[§24.6.1](browser.md#spec-24-6-1)）

标识与 SPEC 概念表一一对应：

| 标识 | 实现 |
| --- | --- |
| `profile_id` | 长期身份。`data_dir/browser/profiles/<profile_id>/` 即真实引擎 profile 目录（chromium `--user-data-dir` / firefox `--profile`），cookies/localStorage/登录态由引擎自己持久化。多身份天然隔离。 |
| `session_id` | 一次实际运行（新 `EntityId` + 派生 `run_id`）。重启后 open 形成新运行身份。 |
| `tab_group`/`tab_id` | 会话内分组与具名标签页；不形成独立登录身份。 |
| `controller_epoch`/`input lease` | 每会话原子 epoch + 互斥 lease（`Agent`/`Human`）；迟到输入携带旧 epoch 即被拒。 |

同 profile 单写者：文件锁（`create_new` + `/proc` PID 存活核对 + 持有者 instance/session 匹配）保证任何时刻只有一个进程/会话以写者身份打开 profile 目录；跨进程/节点接续前核对原 writer 状态。`IdentityFacts` 分别报告持久状态（profile 在盘）、活进程状态（engine 会话）、以及恢复覆盖缺口（见 [§6](subject-runtime.md#spec-6)）。

`storageState`（cookies + localStorage）可显式导出/导入，用于身份迁移；它**不**宣称恢复活页面内存、表单或 WebSocket。凭证/登录态不因记忆同步自动复制（[§24.6.1](browser.md#spec-24-6-1) 末段）。

## 4 观察与动作回执

`observe` 返回结构化 `Observation`：DOM 快照（url/title/具名元素矩形）、内容文本、AX 树（能力可用时；按 `MAX_AX_NODES` 截断）、可选截图（PNG 字节进 CAS，digest 入回执）、content/identity digest。`observation_id` 由内容绑定 digest 派生——同一页面状态重复观察得到同一 id，变化才产生新 id。

每个 `PageAction` 一个 `ActionReceipt`：请求摘要、观察前内容 digest、观察后内容 digest（动作前后各强制一次快照，迟到的旧观察直接被拒）、per-action `error_text`、聚合 `EffectStatus`。未知效果一律 `Unknown` 且带原因，不猜。

## 5 接管（[§24.6.2](browser.md#spec-24-6-2)、[§24.3](desktop-runtime.md#spec-24-3)）

`control` 操作：`acquire`（取 lease+当前 epoch）、`release`、`takeover`（撤销旧 lease、epoch+1，旧输入全部失效）、`hand_back`（同样 bump epoch 交还 agent）、`status`。同一显示域的接管只影响该域的会话，不暂停主体思考；跨域会话独立。`HumanInputObserver` trait 暴露给 display 域实现：检测到人类真实输入时调用 `on_human_input(domain_id)`，该域所有会话的 agent lease 被撤销、epoch+1（协议绑定点由 computer_use 侧接线）。

## 6 崩溃恢复（[§24.6.3](browser.md#spec-24-6-3)）

三种恢复边界分别实现，不混同：

1. **provider 重启、浏览器存活**：chromium profile 的 DevToolsActivePort 仍被服务时，open 直接 reattach 到活进程（新 session_id/run_id，但浏览器状态——含未提交表单——原样保留），不重复执行任何已发生的页面状态。
2. **浏览器崩溃**：活会话协议调用失败即出据 `Unavailable` + 明确错误文本；旧 session/run/epoch/回执关联保留在 Store。`reconcile` 读取冻结输入（CAS 中的原始 `ActionRequest`），按 session_id/profile_id 找到（或重开）会话，**重新观察当前页面**，效果记 `Unknown`——绝不因重开页面自动重发提交/发送/付款。重开产生新 run_id；新观察不抹去原先未知结果。
3. **对账完成的副作用**：`EffectStatus` 与证据（截图、前后 digest、错误文本）一起封存，恢复后由主体基于事实判断，代码不内置自动重试。

Store 记录沿用既有外部操作原语：`arm_external_operation` → 执行 → `seal` 证据 → `record_external_evidence`；handler 失败置 `handler_failed` 不自动重跑（[§41.4](durable-state.md#spec-41-4)）。

## 7 注册与调用路径

`bin/shenko` start 路径注册 5 个原生任务处理器（`browser.session`/`browser.page`/`browser.action`/`browser.control`/`browser.identity`，prompt 进模型可见能力目录），与 `computer_use` 同等待遇。CLI：`shenko browser --file batch.json`，step 形态 `op: identity|session|page|action|control`，同一 batch 内 session 句柄有效。

## 8 明确不做

- 不下载/安装浏览器引擎；不做 Camoufox 反检测适配的核心固定（Camoufox 走同一 `EngineFamily::Firefox` 发现路线即可用）。
- 不恢复活页面 RAM/表单/WebSocket 跨重启续接（SPEC 明确不承诺）。
- 无头/有头模式切换需重开会话并恢复持久状态，不承诺无损热切换。
- 不把 VNC/RDP 端口当共享入口；接管语义只用既有显示域控制锁。

<a id="spec-24-6"></a>
<!-- migrated-contract: SPEC:24.6 -->
## 24.6 原生浏览器能力、持久身份与人类接管

2026-10-26 用户要求浏览器像 OMP 自带能力一样自然，支持多个持久状态及有头/无头运行，人类可以接管有头会话。本节与 [§19.1](repl.md#spec-19-1) 是**设计补充，尚未实现或完成验收**；沿用统一 Node、最小核心、Cordis 插件、能力目录和主委派合同，关联 [#71](https://github.com/cyjin-yl/shenko/issues/71) 及既有 REPL/OS 工作包。

浏览器是原生可发现、可解释的能力：worker 可通过类型化工具或 Eval/REPL facade 打开/复用标签页、导航、观察、交互和取得结构化结果，无需每次通过 shell 临时拼脚本。facade 与底层 adapter 分离；Camofox/Camoufox 是拟适配路线，不固定在核心，也不把反检测特征当作通用访问成功保证。Browser 能力与可替换 kernel 可以组合，但不要求所有 kernel 都执行相同浏览器脚本或具备完整 Node API。

参考 [OMP 固定 browser 文档](https://github.com/can1357/oh-my-pi/blob/9519abfe7d5068868555ef6d9cbe00beccddf519/docs/tools/browser.md)：其 browser 默认在启用 Eval 时提供 facade，并非独立 AgentTool；managed 后端为 Chromium，不是 Camoufox。采用的是自然发现、具名句柄、结构化观察与会话复用的交互思路，不原样移植其工具权限制度或宣称后端已经兼容。

<!-- /migrated-contract: SPEC:24.6 -->

<a id="spec-24-6-1"></a>
<!-- migrated-contract: SPEC:24.6.1 -->
### 24.6.1 身份、运行会话和真实持久化范围

以下为设计概念，不是已发布 API：

| 标识 | 含义 |
| --- | --- |
| profile_id | 长期浏览器身份及其持久状态归属，可存在多个隔离身份 |
| session_id | 一次实际运行的 browser/context 会话；重启后形成新运行身份 |
| tab_group / tab_id | 同一会话内的分组与标签页，不自动形成独立登录身份 |
| controller_epoch / input lease | 当前输入控制权和观察有效期，不是登录状态或 profile 版本 |

默认支持多个持久浏览器身份，按需启动会话，而不是要求所有身份常驻。持久状态、活进程状态和恢复完成分别报告。不同身份的 cookies/storage 隔离；同 profile 只能有一个实际写入所有者，跨进程/节点接续也须核对原 writer 状态，不能同时打开同一 profile 目录写入。

[jo-inc/camofox-browser](https://github.com/jo-inc/camofox-browser/tree/39c82094013480b373df6600d44c7f036f58356e) 与 [daijro/camoufox](https://github.com/daijro/camoufox/tree/8589f980fda690af63181350e7904bd621bf088e) 是不同项目。前者在该快照中按 userId 建独立 context；sessionKey 只是 context 内 tab group，不能作为第二个独立账号身份。[persistence 插件](https://github.com/jo-inc/camofox-browser/blob/39c82094013480b373df6600d44c7f036f58356e/plugins/persistence/README.md) 保存 cookies、localStorage，可选 IndexedDB；这是 storageState 恢复，不是完整浏览器 profile 或 RAM、表单、WebSocket、所有标签页运行态的跨重启恢复。

需要 profile 级持久化时，可另做 Camoufox persistent_context/user_data_dir adapter：[Python API](https://github.com/daijro/camoufox/blob/8589f980fda690af63181350e7904bd621bf088e/python/src/camoufox/sync_api.py) 走 launch_persistent_context；当前 [launch_server](https://github.com/daijro/camoufox/blob/8589f980fda690af63181350e7904bd621bf088e/python/src/camoufox/server.py) 明确拒绝 persistent_context/user_data_dir，不得假定服务端接口直接支持。完整 profile 仍不等于活页面内存恢复；adapter 要明示恢复覆盖、缺口和候选版本。

Cookies/profile 包含敏感登录持久态，不因 [§5.4](nodes.md#spec-5-4) 的记忆同步自动复制凭据、扩大账号或节点访问权。只同步任务/认知不表示已转移浏览器登录身份；实际持久态保全、恢复和迁移按既有凭据/数据合同单独表达。本次设计未创建任何用户凭据。

<!-- /migrated-contract: SPEC:24.6.1 -->

<a id="spec-24-6-2"></a>
<!-- migrated-contract: SPEC:24.6.2 -->
### 24.6.2 virtual-headed 默认与同会话接管

默认采用可显示的 virtual-headed 路线，在实际支持的节点上让 Agent 与人类操作同一 live session；true headless 保留为可选模式。virtual-headed 是虚拟显示上的有头浏览器，不等于 true headless。若节点缺少显示能力，报告真实条件并选择可行 adapter，不按设备名硬拒绝。模式切换可需要重启并恢复持久状态，不承诺 headless/headed 无损热切换或恢复活表单与 WebSocket。

人类接管沿用 [§24.3](desktop-runtime.md#spec-24-3)：暂停旧 Agent 的新输入，撤销旧 controller epoch/lease，使其迟到输入失效；人类交回后取得新观察和新 lease，再继续任务。只暂停受接管显示域的执行，不把整条主体思考改成人工等待，也不把“允许接管”变成每次操作必须人类审批。多个接管者共享同一显示域控制锁，不能同时与 Agent 双写；跨显示域可以独立控制。

可用 [Camofox VNC 插件](https://github.com/jo-inc/camofox-browser/blob/39c82094013480b373df6600d44c7f036f58356e/plugins/vnc/README.md) 等作为 adapter 参考，但不能把 VNC 端口本身当作安全共享入口。接管只向用户显式选定的对象提供受认证、受保护传输的入口，准确限定能看到和控制的 session/显示域；不广泛公开带登录态的桌面，也不能在实际共享整个显示域时声称只暴露一个 tab。认证传输、控制权与观察锁是已有会话边界的实现，不是新增产品决策的人类 gate。

<!-- /migrated-contract: SPEC:24.6.2 -->

<a id="spec-24-6-3"></a>
<!-- migrated-contract: SPEC:24.6.3 -->
### 24.6.3 崩溃恢复与待验收行为

浏览器崩溃或断线后，恢复 profile/storageState 与恢复任务事实分别处理。提交、发送、付款等动作结果未知时先观察/对账，不因为重新打开页面、profile 恢复或 cell replay 自动重发。保留旧 session、输入 epoch、操作及实际回执的关联；新观察不抹去原先未知结果。

后续以固定源码/环境验证：多个身份与 tab group 的登录隔离、正常重启恢复及真实覆盖缺口；人类接管/交回时旧输入失效且无双写；profile 崩溃与未知提交的对账；多 REPL session 的变量保持、中断/取消、并发与迟到输出、失败以及跨节点显式恢复。沿用现有 R0/R1/R2 和效果证据，少量针对性检查即可，不为本次设计新增测试工厂。

上游资料静态核查于 2026-10-26；源码可见能力不等于 Shenko 已集成或实测。本次仅改设计，没有运行浏览器、REPL、VNC 或任何项目源码，没有安装服务、建立登录态或改变权限。

<!-- /migrated-contract: SPEC:24.6.3 -->
