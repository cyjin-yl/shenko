# 浏览器能力功能合同（docs/specs/browser.md）

绑定：SPEC §24.6（原生浏览器能力、持久身份与人类接管）、§19.1（多 kernel 与统一会话生命周期）、§41（插件效果声明、重试/取消/补偿与恢复边界）、§24.3（显示域接管语义）。实现位于 `crates/shenko-exec-host/src/browser/`，运行注册位于 `bin/shenko/src/main.rs`。

本文件是已实现代码的功能合同，不是设计草案。身份/会话/接管/恢复语义以 SPEC §24.6 为准；本文只记录实现如何兑现每一条。

## 1 能力定位

浏览器是原生可发现、可解释的受委派能力：worker 通过类型化原生任务（native task kind）打开/复用标签页、导航、观察、交互并取得结构化结果；本包不新增第二条 agent 循环，不内置自主策略。`shenko browser` CLI 子命令是同一 `BrowserProvider` 的直接驱动面，仅用于原生验证与诊断。

引擎发现只用节点上已安装、路径被钉住的浏览器（`chromium`/`chrome`、`firefox`/`camoufox` 的系统与 PATH 候选，加 `SHENKO_BROWSER_ENGINE` 覆盖），并实测 `--version`。从不自动下载浏览器；无可用引擎时打开会话返回明确错误并报告探测过的位置。默认虚拟显示（Xvfb）+ 有头引擎 = virtual-headed；`SHENKO_BROWSER_DISPLAY` 可指向人类显示域；`mode: headless` 显式真无头。节点无任何显示能力时，`auto` 模式如实降级为 headless 并在 `mode_degradation` 记录原因。

## 2 协议适配层

`protocol.rs` 定义引擎无关 `BrowserProtocol` trait（目标/标签页、导航、JS 求值、点击/输入/滚轮、cookie/storageState、截图、AX、关闭），两种真实适配：

- `protocol_cdp.rs` — Chromium DevTools Protocol（`Target.createTarget` + flatten session、`Page`/`Runtime`/`Input`/`Storage`/`Accessibility`），endpoint 从 DevToolsActivePort 文件读取，端口 0 由浏览器自选，不猜测。
- `protocol_bidi.rs` — WebDriver BiDi（Firefox，`session.new` + browsingContext/script/storage/input 模块），ws URL 从引擎 stderr 解析。

两者共用 `WsClient`：请求/响应按 id 配对、事件进广播 channel、协议级错误文本进 `error_text`（导航失败、JS 异常都不静默为成功）。HTTP helper 支持 chunked 解码用于 CDP 发现。不支持的观察维度如实上报（如 BiDi 的 `ax_available=false, ax_note`），不伪造。

## 3 身份与会话（SPEC §24.6.1）

标识与 SPEC 概念表一一对应：

| 标识 | 实现 |
| --- | --- |
| `profile_id` | 长期身份。`data_dir/browser/profiles/<profile_id>/` 即真实引擎 profile 目录（chromium `--user-data-dir` / firefox `--profile`），cookies/localStorage/登录态由引擎自己持久化。多身份天然隔离。 |
| `session_id` | 一次实际运行（新 `EntityId` + 派生 `run_id`）。重启后 open 形成新运行身份。 |
| `tab_group`/`tab_id` | 会话内分组与具名标签页；不形成独立登录身份。 |
| `controller_epoch`/`input lease` | 每会话原子 epoch + 互斥 lease（`Agent`/`Human`）；迟到输入携带旧 epoch 即被拒。 |

同 profile 单写者：文件锁（`create_new` + `/proc` PID 存活核对 + 持有者 instance/session 匹配）保证任何时刻只有一个进程/会话以写者身份打开 profile 目录；跨进程/节点接续前核对原 writer 状态。`IdentityFacts` 分别报告持久状态（profile 在盘）、活进程状态（engine 会话）、以及恢复覆盖缺口（见 §6）。

`storageState`（cookies + localStorage）可显式导出/导入，用于身份迁移；它**不**宣称恢复活页面内存、表单或 WebSocket。凭证/登录态不因记忆同步自动复制（SPEC §24.6.1 末段）。

## 4 观察与动作回执

`observe` 返回结构化 `Observation`：DOM 快照（url/title/具名元素矩形）、内容文本、AX 树（能力可用时；按 `MAX_AX_NODES` 截断）、可选截图（PNG 字节进 CAS，digest 入回执）、content/identity digest。`observation_id` 由内容绑定 digest 派生——同一页面状态重复观察得到同一 id，变化才产生新 id。

每个 `PageAction` 一个 `ActionReceipt`：请求摘要、观察前内容 digest、观察后内容 digest（动作前后各强制一次快照，迟到的旧观察直接被拒）、per-action `error_text`、聚合 `EffectStatus`。未知效果一律 `Unknown` 且带原因，不猜。

## 5 接管（SPEC §24.6.2 / §24.3）

`control` 操作：`acquire`（取 lease+当前 epoch）、`release`、`takeover`（撤销旧 lease、epoch+1，旧输入全部失效）、`hand_back`（同样 bump epoch 交还 agent）、`status`。同一显示域的接管只影响该域的会话，不暂停主体思考；跨域会话独立。`HumanInputObserver` trait 暴露给 display 域实现：检测到人类真实输入时调用 `on_human_input(domain_id)`，该域所有会话的 agent lease 被撤销、epoch+1（协议绑定点由 computer_use 侧接线）。

## 6 崩溃恢复（SPEC §24.6.3）

三种恢复边界分别实现，不混同：

1. **provider 重启、浏览器存活**：chromium profile 的 DevToolsActivePort 仍被服务时，open 直接 reattach 到活进程（新 session_id/run_id，但浏览器状态——含未提交表单——原样保留），不重复执行任何已发生的页面状态。
2. **浏览器崩溃**：活会话协议调用失败即出据 `Unavailable` + 明确错误文本；旧 session/run/epoch/回执关联保留在 Store。`reconcile` 读取冻结输入（CAS 中的原始 `ActionRequest`），按 session_id/profile_id 找到（或重开）会话，**重新观察当前页面**，效果记 `Unknown`——绝不因重开页面自动重发提交/发送/付款。重开产生新 run_id；新观察不抹去原先未知结果。
3. **对账完成的副作用**：`EffectStatus` 与证据（截图、前后 digest、错误文本）一起封存，恢复后由主体基于事实判断，代码不内置自动重试。

Store 记录沿用既有外部操作原语：`arm_external_operation` → 执行 → `seal` 证据 → `record_external_evidence`；handler 失败置 `handler_failed` 不自动重跑（§41.4）。

## 7 注册与调用路径

`bin/shenko` start 路径注册 5 个原生任务处理器（`browser.session`/`browser.page`/`browser.action`/`browser.control`/`browser.identity`，prompt 进模型可见能力目录），与 `computer_use` 同等待遇。CLI：`shenko browser --file batch.json`，step 形态 `op: identity|session|page|action|control`，同一 batch 内 session 句柄有效。

## 8 明确不做

- 不下载/安装浏览器引擎；不做 Camoufox 反检测适配的核心固定（Camoufox 走同一 `EngineFamily::Firefox` 发现路线即可用）。
- 不恢复活页面 RAM/表单/WebSocket 跨重启续接（SPEC 明确不承诺）。
- 无头/有头模式切换需重开会话并恢复持久状态，不承诺无损热切换。
- 不把 VNC/RDP 端口当共享入口；接管语义只用既有显示域控制锁。
