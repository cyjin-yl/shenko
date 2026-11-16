# BoxLite 插件集成核验

研究范围：官方主线的 Computer Use、桌面/浏览器、REST/SDK、agent 集成及平台边界。只读研究；没有安装、运行 benchmark 或开发；不修改产品主稿。

## 版本与证据边界

- 仓库：[boxlite-ai/boxlite](https://github.com/boxlite-ai/boxlite)。
- 固定主线 commit：[b59b8c1f39e45e49adede56a65749a431d1380e7](https://github.com/boxlite-ai/boxlite/commit/b59b8c1f39e45e49adede56a65749a431d1380e7)，committer 时间 **2026-10-22 14:08:07 UTC**，提交标题 `feat(vmm): add the chipset registry (#2063)`。
- 本轮独立查询 latest release 为 [v0.10.7](https://github.com/boxlite-ai/boxlite/releases/tag/v0.10.7)，**2026-10-18 00:37:38 UTC**，非 prerelease。以下结论锚定上述 main commit，**不把主线源码中新能力自动算作 v0.10.7 已发布能力**。
- 主要证据由 GitHub connector 读取固定 SHA 的真实文件。网页首页检索 ref 为 `turn117view0`；固定文件的 web open 返回 DisabledError，因此不使用 `turn120view0–3`、`turn121view0–1` 作为内容证据。下列固定链接可直接用于复核。
- 本文把“源码已有”“文档声称”“本产品需要的适配”分开；未做本机兼容验证，不承诺性能数字。

## 八项集成结论与局限

### 1. 适合作为可选的完整 Linux 环境后端，不替代基础 WASM 家园

**已核实：**BoxLite 是 Rust runtime 管理的硬件虚拟化 micro-VM；内部运行 guest service 和 OCI container runtime。官方平台矩阵是宿主支持范围，不能把它解释成任意 guest OS。

| 层次 | 本轮证据支持的范围 | 对本产品的意义 |
| --- | --- | --- |
| macOS 宿主 | Apple Silicon ARM64；README 标 macOS 12+ | 可在受支持 Mac 上运行 Linux Box |
| Linux 宿主 | x86_64、ARM64；须可访问 KVM | 可作为本地或远端完整 Linux runtime 节点 |
| Windows 宿主路径 | x86_64 WSL2，并要求 KVM 支持 | 不是原生 Windows hypervisor 后端，也不是 Windows 桌面 guest |
| macOS Intel | README 为 coming soon | 不列作已支持 |
| 桌面 guest | ComputerBox 实际选 Ubuntu XFCE 的 Linux webtop image | 不覆盖原生 Windows 桌面或 macOS GUI 软件 |
| Android/路由器 | 未在正式宿主矩阵中列出 | 可走本产品远端 transport；不能由 ARM64 或 Rust 推断当地可直接启动 VM |

**建议：**映射为可安装的 RuntimeProvider/ExecutionProvider 后端；轻节点可控制另一台运行 BoxLite 的机器。我们的 WASM profile、Android 与任意真实 OS 桌面 provider 继续独立保留。

来源：[固定 README](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/README.md)、[架构概念](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/concepts/README.md)、[ComputerBox 常量](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/constants.py)。

### 2. ComputerBox 已有可复用的 GUI 原语；本产品仍需补状态与输入所有权合同

**已核实：**Python 和 Node/TypeScript 均有真实 ComputerBox 类。默认 image 是 `lscr.io/linuxserver/webtop:ubuntu-xfce`；默认配置为 2 CPU、2048 MiB、1024×768，GUI 服务 HTTP 3000/HTTPS 3001。输入经 `xdotool`，截图经 guest 内 Pillow `ImageGrab`；提供移动、各类点击、拖拽、光标位置、打字、组合键、滚动、屏幕尺寸和 base64 PNG 截图。这是实际 SDK 代码，不只是演示视频。

**边界：**这些默认资源不是测得的最低要求；所查 ComputerBox API 未提供本产品要求的 desktop input lease、controller epoch、stale-frame 拒绝、动作回执或 OS accessibility tree。截图返回的宽高是默认常量，而另有实时 `get_screen_size()`；不能在分辨率变化后只信截图元数据。Node/Python helper 也不能代替原生 macOS/Windows OS 权限。

**建议：**做 ComputerUseProvider adapter，统一观察 ID、实际分辨率/坐标变换、桌面级输入租约、人类接管、未知副作用及执行后的效果核验。复杂 UI/截图模型仍走统一推理 broker。

来源：[Python ComputerBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/computerbox.py)、[Node ComputerBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/node/lib/computerbox.ts)、[桌面示例目录](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/examples/python/05_browser_desktop/README.md)。

### 3. BrowserBox 可复用成熟浏览器协议，但协议与实例能力须分开报告

**已核实：**Python/Node BrowserBox 实际提供 Playwright server 模式，支持 chromium、firefox、webkit；另一条 direct 模式采用 Chromium CDP 或 Firefox WebDriver BiDi。源码明确两种模式对同一实例互斥；WebKit 不支持 direct Puppeteer 路径。浏览器在 Linux image 内运行，不是宿主已有登录会话。

**边界：**返回值含 localhost 地址；代码有 3000/9222 转发安排。Python 文档还列出 Firefox headless `newPage()` 的已知限制。不能把“有 CDP”推成所有浏览器协议行为一致，也不能把浏览器 DOM/AX 控制推成完整 OS AX。README 的早期使用片段与当前方法异步签名有漂移，应以 pinned 实现和契约测试为准。

**建议：**独立 BrowserProtocol adapter，声明具体 engine、transport、browser/context identity；由 Playwright/CDP 提供可用的结构化观察，保留截图路径。身份/登录数据、恢复与 scope 仍由本产品统一管理。

来源：[Python BrowserBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/browserbox.py)、[Node BrowserBox](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/node/lib/browserbox.ts)。

### 4. 自托管 REST 已有真实实现；可承接远端节点与 agent seat，不能假定每个服务端能力相同

**已核实：**`boxlite serve` 是 Rust/Axum server，持有 BoxliteRuntime，提供 Box CRUD/start/stop、exec/status/signal/kill/resize、WebSocket attach、tar 文件传输、metrics、snapshot/restore、clone/export/import 等。配置端点提供部分能力发现。无 API key 时只允许 loopback permissive 模式，非 loopback 绑定要求 key；API key 是 Bearer，不等同于本产品 person/task/scope 授权。

**SSH 的准确范围：**此 commit 的 Rust `LiteBox::ssh()` 和本地 `boxlite serve` 的 POST/GET/DELETE `/v1/boxes/{box_id}/ssh` 已有实现，包括真实 handlers；不能因为跟踪 issue 仍 open 就说完全未实现。官方 SSH guide 同时明确：CLI 和其他语言 SDK 当前没有 SSH control API；其他部署可能 404/Unsupported，而且没有 SSH 专门能力协商。配置 SSH 不会自动发布端口；每次 configure 都重启 listener，不应盲目重试。SshConfig 的 host private key 是 **guest SSH server 的主机身份**，不是 Agent 向别人机器登录时的 client private key，更不是 ssh-wasm 实现。

**建议：**以 REST/SSH/PTY 接到 RemoteAgentProvider，保存 box/execution/session 标识和 lease；实际 Claude/OMP 协议仍由各 adapter 处理。跨渠道决策回灌、联系人策略与唯一主体都留在我们自己的合同中。

来源：[serve 实现说明](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/cli/src/commands/serve/README.md)、[SSH guide](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ssh.md)、[SSH 实际 handlers](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/cli/src/commands/serve/handlers/ssh.rs)。

### 5. 高层桌面/浏览器 helper 当前并非透明远端兼容；这是具体适配缺口

**静态源码证据：**Python/Node ComputerBox 与 BrowserBox 构造器都会自动填入非空 host `ports`；BrowserBox endpoints 返回 localhost。与此同时，同 commit 的 REST runtime `sanitize_remote()` 明确拒绝任何非空 `ports`，提示改用 `box.network.tunnel(port)`。因此，直接把这些 helper 的 runtime 换成 REST，不能据此宣称远程 GUI 可用；这个结论来自两侧实现约束，尚未实际运行复现。

REST 同时拒绝 host bind mounts、只读 managed volumes，以及若干仅由本地 runtime 管理的 security/kernel/network 参数。远端 mount 不能把控制端的任意本地路径假装成服务端路径。

**建议路线：**A. 节点 companion 在 BoxLite 宿主本地使用高层 helper，对我们暴露 ComputerUseProvider；或 B. 以底层远端 box、显式服务启动和 network.tunnel 构建专门 adapter。无论哪条都按实际 server 能力协商，不复用 localhost 假设。需把 GUI endpoint 的认证、生命周期和有效期纳入 adapter；本轮不提前宣布具体组合已验收。

来源：[REST runtime，sanitize_remote](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/runtime.rs)、上文两种语言的 ComputerBox/BrowserBox 实现、[Rust API 中 VolumeSpec 与 tunnels](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/reference/rust/README.md)。

### 6. Rust 核心与 Python/JS 生态适合双层插件，但“Rust”不等于轻量 WASM 或全设备二进制

**已核实：**Rust SDK 是核心 Tokio async API；Python 是 PyO3 原生绑定（Python 3.10+）；Node 是 napi-rs + TypeScript convenience wrappers（Node 18+）。Node 发布 targets 明列 Apple ARM64 与 Linux x86_64/ARM64。仓库另有 Go/CGO 与 C SDK。Rust crate 有 `rest` feature；默认 feature 同时包括 embedded-runtime/krunfw/e2fsprogs/bubblewrap，并有其他 native/OS 依赖，不能把链接完整 crate 自动称为轻 thin client。

**版本边界：**README 说 macOS 12+，但当前 Python wheel 构建配置明确 `MACOSX_DEPLOYMENT_TARGET=14.0`、`macosx_14_0_arm64`，注释提到 libkrunfw 最低 14.0。正式安装支持必须按实际发行件校验，不能仅照抄 README 较宽的系统下限。Python 打包配置还跳过 musllinux；不因此否定自行移植，但不能承诺路由器包现成。

**建议：**默认主内核仅保留我们的 provider 协议。较大设备安装本地 BoxLite adapter/companion，小设备使用远端协议。开发验收分别测 native embedding、独立进程与 HTTP client 的打包体积、常驻内存及目标平台；不继承文档中未经本轮测量的“<100ms”“50–100MB”等性能宣传。

来源：[Rust Cargo features](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/Cargo.toml)、[Python SDK](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/README.md)、[Python 打包配置](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/pyproject.toml)、[Node package targets](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/node/package.json)。

### 7. 已有 harness 承载与 Skills 插件，可借鉴适配方式；它不是本产品的个人 Agent 主体

**已核实：**官方示例含 OpenAI/MiniMax tool loop、Claude Code stream-json 多轮通信、SkillBox、OpenClaw gateway。SkillBox 类可以运行 Claude Code、安装 skills、保留同进程多轮会话，并带桌面。现有 Codex/Claude BoxLite plugin v0.1 的定位是应用构建/部署、返回已注册 HTTPS URL、重复部署保留 Box/data；README 明确它由 skills 和可选 CLI helpers 组成，**没有 MCP server、lifecycle hook 或应用模板**。这不是一个我们可以原样装上就获得所有 Computer Use 原生 tools 的插件。

**边界：**SkillBox 示例要求 Claude OAuth token，并实际使用 skip-permissions 模式；这些示例不构成多种模型共享 broker、可解释 DecisionRequest、人格连续性或远程权限委托的产品合同。其默认 cleanup 也不等于持久的个人家园。

**建议：**借鉴“可发现的能力说明 + 可选辅助工具 + 按需细分文档”的插件分发方式，把完整环境当孩子的执行场所。我们的主 loop、联系人与群聊、原文记忆、自然联系策略、动态 UI、总预算及持续自改进不由某个内嵌 coding harness 接管。可装 Claude/OMP 等作为有限远端 seat；实际模型请求仍要纳入 broker 覆盖核算。

来源：[官方 agent 示例索引](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/examples/python/06_ai_agents/README.md)、[SkillBox 实现](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/skillbox.py)、[BoxLite plugin README](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/plugins/boxlite/README.md)。

### 8. 集成时要显式兑现持久、取消、并发和副作用合同，不能继承示例默认值

**已核实：**Python 文档明确默认 Box 在 stop 时删除；`auto_delete=0` 才保留 Box/QCOW2 供 stop/restart。SkillBox 的 `auto_remove=True` 也为默认。官方 AI integration guide 提醒：`asyncio.wait_for()` 只取消 coroutine，不会自动杀死 guest process，需要 execution.kill 等清理。一个 VM 可并发多个 exec，但它们共享同一文件与进程环境；独立 VM 才有相应隔离边界。

**建议：**持久家园的 adapter 必须显式指定保留策略、把 box/generation/task 绑定，并用本产品的 checkpoint/恢复协议区别磁盘状态、活进程和已发生的外部效果。研究孩子可以共享 VM 以摊薄 boot 成本，但按任务文件区、写入 lease 与 CPU/内存总额管理；需要互相隔离的实验才 fork/clone/新 VM。工具取消要等待实际终止/未知状态确认，再释放执行容量；BoxLite 的 VM/CPU 配额不能代替模型请求并发、KV 驻留与 cache/费用账本。涉及 VM 存储/快照完整语义由另一项核心研究进一步核验，本报告不把持久磁盘误称为完整 RAM checkpoint。

来源：[Python SDK 持久化与默认 cleanup](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/README.md)、[AI agent integration guide](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ai-agent-integration.md)、[SkillBox defaults](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/skillbox.py)。

## 必须进入集成验收的五点

| ID | 场景与通过标准 |
| --- | --- |
| BX-I01 | **本地与远端真实闭环**：分别创建 Linux desktop/browser，实际完成观察—输入—再观察；远端使用所选 companion/tunnel 实现，不把 helper 接受 runtime 参数当成已兼容。远端非空 ports、host bind mount 和只读 managed volume 的拒绝必须显示为实际能力，不静默改成别的权限。 |
| BX-I02 | **GUI 状态与接管**：改变屏幕尺寸、移动窗口、断线重连、人工接管，核对实际截图宽高和 controller/observation epoch；旧坐标、旧 lease 不再输入，整个共享桌面只有一个机器输入者。 |
| BX-I03 | **持久和取消**：显式保留 Box，stop/restart 后验证文件、任务定位与凭据引用；分别验证进程是否仍存活，不冒称完整 RAM resume。取消长 exec 后观测其实际终止或明确 unknown，再释放执行容量；重连不重复外部提交。 |
| BX-I04 | **平台与服务能力**：按实际分发包验证 Mac 最低版本、Linux/KVM、WSL2 及 Node/Python 版本；Native Windows/macOS guest、Android local VM 未验证则不得打勾。Rust/REST guest SSH configure/status/disable 实测，不从本地 serve 推断所有 cloud server 支持，也不冒称 ssh-wasm client。 |
| BX-I05 | **有限 worker 与推理覆盖**：在 Box 内运行 Claude/OMP 或研究孩子，确认仍受唯一主体委派、task/scope/lease 与实际授权约束；它们及视觉处理若使用同一受控本地 API，全部经 broker，C=1 不因 VM 数量或递归子 Agent 绕过。外部未纳管模型请求单独标明，不能合并成硬全局覆盖。 |

## 固定 SHA 核心行号索引

以下行号均属于 b59b8c1f39e45e49adede56a65749a431d1380e7，可供主稿选取更精确引用。

| 证据 | 文件与行号 |
| --- | --- |
| Linux desktop image 与默认资源 | [constants.py L10](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/constants.py#L10)，L10–17 |
| ComputerBox 自动 host ports；截图尺寸常量 | [computerbox.py L76](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/computerbox.py#L76)，L76–87；L136–164；实时尺寸 L291 起 |
| BrowserBox 模式互斥、默认 ports、localhost endpoint | [browserbox.py L26](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/boxlite/browserbox.py#L26)，L26；L168–179；L499–521 |
| REST 确实拒绝 ports/mount，而不是文档提醒 | [rest/runtime.rs L204](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/src/boxlite/src/rest/runtime.rs#L204)，L204–210、L283、L299；create 路径调用 L318/L352 |
| SSH Rust/REST 支持；CLI 与其他 SDK 无 control API | [ssh.md L3](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ssh.md#L3)，L3–24、L64–74；真实 handlers L22/44/60 起 |
| Python macOS wheel 实际部署 target 14.0 | [pyproject.toml L144](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/pyproject.toml#L144) |
| 既有插件是 CLI/skills 分发，无 MCP server/hook | [plugins README L35](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/plugins/boxlite/README.md#L35) |
| stop 默认删除；wait_for 不杀 guest process | [Python README L711](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/sdks/python/README.md#L711)，L711–718；[integration guide L256](https://github.com/boxlite-ai/boxlite/blob/b59b8c1f39e45e49adede56a65749a431d1380e7/docs/guides/ai-agent-integration.md#L256) |

## 对 SPEC 的收敛建议

将 BoxLite 列为**优先验证的可选完整 Linux runtime / Computer Use 插件后端**，附上述固定版本能力表。它可以显著减少“运行真实 Linux 工具、GUI 与现成 agent”的底层工作；当前不替代基础 WASM、不覆盖全部 OS guest、不免除远端 GUI 适配、不提供个人主体或全局推理调度合同。本轮只提出上述八项集成结论，没有安装或开发。

