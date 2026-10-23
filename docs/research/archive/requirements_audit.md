# 产品 SPEC 需求覆盖审查

状态：依据用户原文及后续新增要求提取，更新至用户明确允许 Agent 自主改写任何部分（包含 kernel/rescue），明确“Pi for coding agent，本项目 for personal agent 的最小框架”，将 Linux/Windows/macOS 的 Computer Use 列为正式插件能力，并追加参考 OpenAI Dynamic Response 的动态 UI 要求。新增约束已经是用户要求；第一轮其他规格选择尚未收到答案。所有未被新增要求确认的团队推荐默认值仍为**未确认假设**，不能被写成用户已批准的裁决。本文件是内部研究/覆盖审查材料，未进入开发。

## 使用规则

- `R001–R091` 是用户需求的可追踪编号；每个编号应指向 SPEC 中的正文条款、验收或明确待裁决项。精简正文映射另见 `requirements_traceability.md`。
- “待验证”不是“删除”，“有技术障碍”不是“用户接受降级”。候选替代方案必须与原需求同时保留，写明所差的能力。
- 以下原文短引仅为定位；完整原文应另行原样保留。原文内的因果假设可被研究纠正，但由该假设表达的产品意图不能悄悄消失。
- 规格状态建议使用：`用户明确提出 / 待用户裁决 / 待技术验证 / 推荐方案（未确认）`。不要用同一个“支持”状态混淆它们。
- **最新要求优先：**任何早期笔记中“rescue/core 永久不可改”“自主改核心尚未授权”或“research reset/semantic forgetting 可以删除原始历史”的建议均已被后续要求覆盖。行为合同需要持续满足，不等于必须永久保留某一代实现源码。用户未禁止 owner 明确请求真实数据删除；真实删除与研究重启/语义遗忘必须分成不同操作，不能以本审查推断其权限被取消。

## 需求矩阵

| ID | 原文定位 | 不可遗漏的产品含义 | SPEC 中应出现的验收/裁决点 |
|---|---|---|---|
| R001 | “类似 AstrBot 的个人/群聊可用的 Agent Bot” | 首先是可个人使用、也可群聊使用的 Agent Bot；不是只面向开发者的 CLI，也不是只有 Web 私聊 | 个人会话与群会话都列用户旅程；群内多说话人、线程/回复对象、身份和会话边界有规则 |
| R002 | “使用 2026 最新潮的技术” | 研究和设计时间锚定 2026 年 10 月的最新可验证技术 | 记录研究截止时间、版本/commit、已发布与未证实状态；不能用旧知识排除已出现的能力，也不能编造“最新” |
| R003 | “自己持久的运行环境” | Agent 拥有可长期延续的环境、文件和工具状态 | 对重启、升级、runtime crash 的持久语义作定义；进程内存是否持续恢复单列待裁决，不得直接把“持久”减为临时目录 |
| R004 | “一个 Linux / POSIX 兼容的 wasm 沙箱” | 默认执行环境要基于 WASM，并有 Linux/POSIX 兼容目标 | 明确 WASI/WASIX/Linux binary 三种兼容级别；真实 shell/process/socket/TTY/fs 行为验收；不能用一个只支持几条命令的假 shell 直接交差 |
| R005 | “可以往里面挂载任意路径” | 可配置任意宿主可访问路径的挂载，不能限制为固定 workspace | 挂载接口、路径映射、RO/RW/COW、动态增加/移除、宿主权限边界；“任意”不等于绕过 OS 权限，但也不能被默认路径白名单无声缩小 |
| R006 | “自由记录很多文档啊，记忆啊的东西” | 自主创建、组织、修改和检索文件化文档/记忆 | 文件层是正式能力；能够持久写入、重命名、组织目录、查询，且不是仅允许写一份 MEMORY.md |
| R007 | “而且还能运行工具” | 环境是实际可执行工具的计算环境 | Exec、stdin/stdout/stderr、exit、流式输出、取消、超时、后台任务等有协议，不只是虚拟文件存储 |
| R008 | “通用的接口…toybox…gcc-wasm 等等任意工具” | 通用工具/包安装接口，涵盖基础工具与较重编译器；不是手写固定工具集 | 包 ABI/依赖/entrypoint/能力描述；toybox 与 gcc-wasm 是明确举例，应保留兼容性条目。GCC host/target 含义可追问；不能默默改成只有 Clang |
| R009 | “它自己也有这个能力” | Agent 能自主安装、配置、更新所需工具 | Agent 可调用安装/构建/升级/回滚流程；人类 UI 操作不是唯一安装途径 |
| R010 | “兼容任意本地记忆系统…linux/posix 兼容” | 能接现有本地 memory 方案，不锁死内置 memory backend | 适配协议与依赖兼容表；POSIX 即自动兼容的因果假设应纠正，但“支持外部本地记忆系统”不能删。哪些需要移植/SSH/container 明示 |
| R011 | “沙箱之外…最小化的 agent software…核心很小，不容易损坏” | 宿主控制面尽可能小、稳定，功能尽量在可替换环境/插件中 | 必需宿主职责有清单与理由；避免把包管理、所有连接器、大型浏览器/Node 栈无条件塞进常驻内核 |
| R012 | “wasm 沙箱 down…自动回滚回最初版本的沙箱” | 无人干预的故障检测与恢复，最终可回到初始已知可启动环境 | last-known-good 与 factory fallback 两者都覆盖；健康检查失败、坏升级、循环崩溃、恢复日志；不能只给人工恢复按钮 |
| R013 | “类似 git 的版本管理…更适合…二进制文件管理” | 环境/文件状态可版本化，且对二进制友好 | snapshot、历史、restore、branch、内容复用、存储上限/GC、损坏处理；不能只用普通 Git 跟踪全部二进制来代替这项要求 |
| R014 | “允许接入 git 仓库作为记忆和知识库…支持认证…gh-cli wasm，或者 gitea cli wasm…私密记忆” | Git memory/knowledge repo 一等支持，认证和隐私必需；用户明确要求关注 WASM 内 GitHub/Gitea CLI | 私有 GitHub/Gitea clone/pull/commit/push/auth expiry；gh/tea WASM 真正实现、专用 host imports 与限制明示；只支持公开 repo 或仅 REST read 不合格 |
| R015 | “默认初始化…初始 wasm 沙箱里的一个挂载在主机目录上的路径 git 作为记忆” | 出厂默认 memory 是宿主目录持久挂载到初始 WASM 环境的 Git 仓库 | 首次启动路径创建、Git init、guest mount、读写和跨恢复保留；无 remote 时本地正常工作。不能改成数据库唯一记忆或云端 repo 必需 |
| R016 | “需要的时候…generate ssh private key” | Agent 可按需要生成自己的 SSH 身份密钥 | keygen 可由 Agent 触发；密钥持久化、轮换/撤销、恢复身份不漂移；不能只接受人类预先提供的私钥 |
| R017 | “人类操作界面暴露自己的 ssh pubkey” | 人类能拿到 Agent 公钥并授权其连接机器 | UI 展示/复制/下载完整公钥及 fingerprint；不是只显示“已配置 SSH” |
| R018 | “ssh 到人类的各种机器…全功能容器…不同的属于它的节点” | SSH 是远程执行、多节点与未来容器接入的正式路径 | 多 target inventory、文件访问/传输、远端命令、连接状态、target 身份；保留后续容器与 Agent 自有节点拓展点 |
| R019 | “ssh wasm 需要支持哦” | 用户特别强调 SSH WASM 支持，不能被“有 SSH 功能”覆盖掉 | 指定真实 WASM SSH 客户端所需流程与 port 验收；Rust host broker 是候选补充/替代，必须待裁决，不能静默视为等价完成 |
| R020 | “用 nix 或者 nix like 的系统管理包” | 包管理采用声明式/可重现/可回滚的 Nix 或类似语义 | 包锁、依赖闭包、不可变存储、generation、原子激活/回滚；普通下载执行器或 npm install 本身不自动满足 |
| R021 | “长期运行…负担…低，甚至能跑在路由器上” | 稳定轻量与 router 可部署是目标 | 参考设备、最低 arch/RAM/storage、idle/active/peak 指标；目标未知要追问，不能一边默认大型 runtime 一边宣称轻量 |
| R022 | “像 Codex 的 budget compact…从历史 session 读取和回忆…真相源” | 历史会话完整保留，compact 后还能定位并读取原始证据 | compact summary 不能替代原始 session；消息/工具结果 stable IDs、引用、搜索、分页展开、budget-aware recall；同名官方功能是否公开可核验与产品需求分开 |
| R023 | “考虑…layerfs…wasm 版本或者裁剪版本…后续的沙箱里” | 深研 LayerFS，评估 WASM/裁剪路线，并保留未来完整沙箱集成 | 上游结构、持久化现状、可复用层、许可证/接口、FUSE/Materialize 区别；不能因不能直接编译就把整个方向删除 |
| R024 | “全自主的，永远运行的…人类不在…默默维护自己，执行一些事物” | 人不在线时仍自主持续生活/工作、维护与推进任务 | 常驻调度、主动任务发现/队列、维护、恢复、进度、长期目标、休眠唤醒及停机语义；不能减为只有用户发消息才运行的被动 Bot |
| R025 | “OpenAI Dot 和 Claude Duet 和 Meta Muse…竞品详细调研…更加自主化一点” | 逐个具名竞品调研并解释差异化，更高自主性是目标 | 每个名称辨识/官方事实/无法核验状态；功能与 UX 对比；不能用其他产品偷换，也不能因为无法证实就不在报告中列项 |
| R026 | “极致隐私和本地 AI 设计” | local-first/privacy-first 是架构原则 | 数据位置、离线能力、私密会话/仓库、凭据、遥测、云模型是否可选；默认远程托管/隐式上传不能当未说明实现细节 |
| R027 | “详细设置…API 的最大并发数量、上下文窗口” | 每个模型服务可配置真实能力与资源预算 | endpoint/model 并发、context/output token 限制、共享硬件 budget、配置校验；不是单一全站并发常数 |
| R028 | “防止多并发打死本地 API，走全局锁【按请求紧急状态进行排序】” | 所有推理调用受统一 admission control 与紧急优先级调度 | 主 Agent、subagents、memory/embedding、compaction/后台任务的模型请求都可追踪入队；共享 endpoint 的全局含义、priority、fairness、取消、deadlock 明确 |
| R029 | “用 rust 编写它的核心功能” | 核心实现语言 Rust | Rust 核心边界、可选非 Rust runtime/sidecar 的关系明确；不能直接以 TypeScript Agent framework 作为完整核心交付 |
| R030 | “webserver…基于 proto ui 的标准的类似竞品的界面” | 提供 Webserver 和正式 Proto-UI Web 操作界面 | 原生管理/交互前端覆盖聊天、任务、记忆、环境、权限/连接、恢复；不能只有 HTTP API 或 CLI |
| R031 | “活灵活现，let layout talks…2026 年 10 月最新最酷的设计规范” | UX 要具有鲜明状态表达、动态空间和当期设计质感 | 页面信息架构、状态/交互/motion 语言、响应式与无障碍；用布局表达任务/环境状态。不能只套默认管理后台或把“酷”简化为过度玻璃效果 |
| R032 | “proto-ui 最新 0.3.0 rc 主线 + shadcn…及时更新…文档里要求每次都 bump 到最新主线” | 明确 Proto-UI 0.3.0 RC 主线 + shadcn 路线，并要求持续跟最新主线 | 版本核验、pin 到具体主线 SHA、每轮开发/升级检查与 bump 条款、回归/失败证据；不能只锁死当前版本不再更新。具体“每次”的触发解释仍待裁决 |
| R033 | “后续替换成 proto-ui compiler 版本的插桩适配空间” | 预留 compiler 版迁移能力 | UI semantic schema/render adapter、runtime bindings、compiler seam 与迁移边界；仅写“以后可重构”不合格 |
| R034 | “deepseek harness…参考对象” | 单独研究 DeepSeek Harness 产品/执行模型 | 明确仓库、版本、工具/插件/执行机制借鉴；R042 另补扩展性比较，不得只在来源列表点名 |
| R035 | “iLinkBot…官方机器人 API / NapCat…我们的 napcat-cli…QQ bot、飞书…OpenClaw…非常多东西” | 中国平台接入与广泛连接器是核心能力；具名路径需保留 | iLinkBot、QQ 官方 API、NapCat/napcat-cli、飞书逐项适配契约与状态；OpenClaw 连接范围研究；不能默认只有 Slack/Telegram 或通用 webhook |
| R036 | “不要…限制太多。默认全允许对话【管理员权限则需要手动设置】” | 对话默认开放；管理员权限明确由人类设置 | 默认接入/触发/群聊策略、管理员登记和操作权限；不得默认 DM pairing、群 allowlist、只有 admin 可聊；对话权限与机器权限分开 |
| R037 | “足够轻，开 subagent 递归搜索、研究和 deepresearch 的代价足够小…深度搜索…合适的时候收敛” | 递归多 Agent 研究既低开销也能深入和收敛 | 子任务生命周期、上下文复用、递归/预算、停止条件、覆盖/冲突证据、汇总；不能只支持固定深度的一次分叉，也不能无限烧 token |
| R038 | “layerfs 那样能试探多种方案，而且不会撞全局并发上限锁” | 多路径探索与全局资源协调同时成立 | 研究分支/fork/试验/比较/合并；parent wait 不持有推理 permit、queue 能推进、探索计划不越全局 cap；不能以“并发上限1”禁用所有并行架构 |
| R039 | “kv cache slot 透传…管理多并发时的上下文…cache miss…prefill…本地AI几乎不可用” | 对有能力的 backend 透传 KV slot/affinity，降低多会话 prefill 损耗 | slot/sequence lease、prefix key、capability discovery、保留/释放/迁移语义、cache hit/prefill 指标；通用 OpenAI-compatible API 本身不保证 slot 支持，不能只写“开启缓存” |
| R040 | “Ninfer, Strata 搭配使用的中英文教程” | 两个具名本地推理生态都要有中文和英文教程 | 先确认项目 identity/版本；分别给配置、启动、context/concurrency/KV、排障、验收路径。产品未开发时教程应明确为计划/示例，不编造可执行命令 |
| R041 | “减少本地 agent 的思考难度…插件或其他手段 提高 ssh 文件访问 ssh 命令执行…memory…工具” | 给较弱本地模型高质量、明确语义的工具层，而非强迫它每步拼 shell | typed ssh.file/read/write/search、ssh.exec、memory search/open/update 等；输出预算、路径/错误语义、发现文档；shell 仍可用，但不是唯一路径 |
| R042 | “oh-my-pi…大量参考研究…pi 和 deepseek harness 的可扩展性…cordis…rust版本” | 深研 oh-my-pi、pi、DeepSeek Harness 的可扩展性与 cordis-rs | 每个具名项目的实现、插件、工具、会话/代理结构比较；cordis-rs URL 正确；不能笼统说“支持插件”就跳过详细调研 |
| R043 | “会话记录…SQLite FTS5 之类…大量参考同行最新潮性能最好的设计” | 会话持久存储与全文索引是正式子系统 | SQLite/FTS5 候选、原始事件/全文索引分离、中文检索、工具结果/附件、namespace、迁移/备份与同行比较；不能只保存 summary 或只用向量库 |
| R044 | “内核…各种设备…wasm…部署到 Android…稳定” | 多设备内核和 Android 是明确部署目标 | target matrix、Android 包装/权限/后台生命周期、跨设备数据和身份、资源 profile；不能拿“Web UI 能用手机访问”冒充 Agent 在 Android 本地运行 |
| R045 | “需求一字不落地复述…深度研究，grill me with spec…完整的产品 SPEC，先不进入开发” | 交付流程必须依次覆盖原文复述、深研、主动规格追问、完整 SPEC；当前不得开发 | 原文已逐字复述；研究证据、追问清单/答复、完整条款与待决日志齐全；未返回答案只形成待裁决草案，不能声称规格获批或直接创建实现 |

### 新增硬约束：持续主循环与委派执行

以下 R046–R054 根据父 Agent 转交的用户新增要求提取；定位栏为需求摘要，不伪装为逐字引语。最终完整原文存档应直接使用父 Agent 可见的用户原消息。

| ID | 新增要求定位 | 不可遗漏的产品含义 | SPEC 中应出现的验收/裁决点 |
|---|---|---|---|
| R046 | proactive thought loop 单线程 | 主体只有一个顺序的思考/决策流；多个子 Agent 不意味着同时存在多个主意识控制流 | 以 single-writer controller/actor 定义逻辑单线程；同一主身份不并发运行两个决策 turn。Rust runtime 的 I/O 线程、DB worker 不受此逻辑限制 |
| R047 | 永不停止思考下一步 | 主体持续主动思考下一步，不以“当前没人发消息/没有显式任务”作为终止条件 | 主 loop 有自主 next-action/探索选择，委派后继续考虑下一步；不能默认退化为 event-only/crontab Bot。物理断电/模型不可用、显式停止以及资源排队的真实状态必须说明；任何 idle backoff 策略仍需裁决，不可悄悄替代持续思考 |
| R048 | 鼓励 handoff 子 Agent 探索 | 主循环主动识别可委派工作，并将探索交给子 Agent | 异步 handoff、任务契约、输入证据、结果回执、继续/取消/收敛；主循环等待结果时不占有模型 permit 或阻断全局队列 |
| R049 | 鼓励子 Agent 自我改进 | 自我改进是允许且鼓励的子 Agent 工作，而不是禁止的未来方向 | 代码/指令/插件/工具改进在独立候选 workspace/generation 内进行；验证、发布与回滚协议；不可把“自我改进”减成仅写建议文档 |
| R050 | 版本、任务进度、记忆机制发生于 subagent | 对文件版本、任务状态报告和记忆实现的实质操作由子 Agent 承担 | version worker、task/progress worker、memory worker 的职责与写权限明确；主 loop 不直接 Git commit、不直接编辑任务文件、不直接维护 FTS 或记忆仓库 |
| R051 | 主 loop 只委派思考、save/read memory、指派 subagent | 主体公开能力集合极窄：语义记忆读写与委派，不持有所有通用执行工具 | controller tool allowlist 以能力级别实现；`memory.read/query/save` 接收语义对象/查询，`agent.handoff/status/cancel` 管理委派；具体工具名是规格建议、未确认实现。不能通过插件自动把 exec/file/network 工具注入主 loop |
| R052 | 主 loop 不能直接操作/写文件 | 主循环不直接调用文件系统、shell、Git、SSH 或编辑工具；所有实质执行委派 | 既阻止任意路径 read/write/edit，也阻止通过 memory API 路径字段、SQL、任意程序、万能工具转发绕回直接文件操作。工具/协议约束应由 runtime 执行，不能只靠提示词 |
| R053 | 人格追加于核心长 system 后 | 人格有明确可定制位置，但核心系统协议先于人格并保持稳定 | Prompt composition = core instructions/protocol + appended persona + structured runtime context；记录 core/persona version；不能把人格替换成唯一 system，也不能因主题/人格切换丢失核心协议 |
| R054 | 人格不能影响工具协议 | 人格改变表达与偏好，不改变 capability、schema、权限、调度和持久化语义 | persona 内容不能注册工具、提升权限、绕过主循环文件禁令、改变工具返回格式；protocol/policy 校验独立于 persona 文本，更新人格需有兼容性检查 |

### 后续需求：消息、KV、记忆、远程 Agent、插件与完整自改

以下 R055–R091 根据父 Agent 转交的用户后续消息及当前主稿核对提取。定位栏是短摘要，**不是伪造逐字引语**；正式原文附录须由父 Agent 保留用户原消息。验收栏中的实现安排是覆盖要求的建议，不意味着用户已经确认具体协议、进程布局或性能数字。

| ID | 后续要求定位 | 不可遗漏的产品含义 | SPEC 中应出现的验收/裁决点 |
|---|---|---|---|
| R055 | 外部消息几乎总是注入主体 | 各消息源汇入同一个持续主 loop，外部消息不是另开一个自主主体 | Durable inbox、顺序消费和忙时排队；不能以渠道各有一条主 loop 代替；同一输入不能形成重复决策 |
| R056 | 消息带别名、ID、来源和内容 | 输入是有身份和上下文的结构化事件，支持富文本与附件 | provider/account/channel/message IDs、sender alias/stable ID、时间、回复关系、内容及附件引用；不只拼一段无来源文本 |
| R057 | 消息 provider 插件化 | 聊天、系统通知和未来媒体来源可独立扩展 | 收发、ack/cursor、重连、去重、附件、生命周期接口；新增消息源不要求重写核心 |
| R058 | API provider 插件化 | 模型/API 接入可动态选择、替换与更新 | model discovery、上下文/并发能力、请求/取消、用量和 KV 扩展；每个 provider 的推理仍进入统一 broker |
| R059 | 认识人的编号与身份插件 | 持续认识联系人，建立稳定 PersonID、别名及多个平台账号的映射 | 认识人编号、来源、置信、作用域、合并/拆分；昵称相同不能自动当同一人；身份插件可替换 |
| R060 | 可选声纹识别 | 可通过插件或自主发展补充说话人身份线索 | candidate/unknown、匹配置信与来源；声纹不是天然可靠的管理员认证；保留无声纹时完整使用路径 |
| R061 | C=1 仍保留多个上下文 | 计算并发和上下文驻留是两种资源，单并发不等于只能一个 session | 主体/多个孩子交错生成，释放 compute 不清空所有 KV；必须区分 backend 可实现能力与 advisory 目标 |
| R062 | 理想条件下自动算法应分到主/子各半 | 在驻留需求和成本适合均分时，算法无需用户手设比例即可形成主体约一半、所有 children 集体约另一半 | 无手动比例的适合均分负载自动收敛到约 50/50，需求变化后自适应；单位、page 舍入、headroom、淘汰和保证等级清楚。静态 50/50 跑通不足，不要求所有场景固定均分，也不是每个 child 各拿一半 |
| R063 | 自动分配，主 Agent 可调策略 | 支持自动 KV 分配，也允许主体根据任务调整主/子份额与策略 | 主体通过语义策略意图或委派调整，不获得通用文件/执行工具；策略变更有 revision，不篡改真实 capacity 或取消状态 |
| R064 | 未填写容量时自动发现 | 不要求用户先掌握所有后端内部参数 | 正式 metadata/metrics 自动探测，标明 exact/estimated/unknown；未知不是零或无穷；不能虚报硬 50/50 已落实 |
| R065 | 主动遗忘/重新开始研究论文 | 研究失败轨迹和过多历史可能妨碍探索，支持新 worker/session 从有限已验证种子重新开始 | 纳入《Scaling Discovery Requires Forgetting》arXiv:2610.20563；区分论文实验与产品推断，reset 的输入、候选状态、预算、收敛及 cold-prefill 成本明确 |
| R066 | reset/语义 forget 不删除原始历史 | 研究重启或语义遗忘改变当前可见上下文/检索选择，保留底层 session 真相源 | 原事件、工具结果和来源可追溯；reset/semantic forget 不调用 erase。owner 明确真实删除是独立意图/协议，用户未禁止该能力；不能把二者混同或臆造绝对永久保留要求 |
| R067 | Proactive Memory Agent 论文 | 研究独立 memory Agent 维护结构化 bank，在必要时提醒或保持沉默 | 纳入《Remember When It Matters》arXiv:2607.08716v1；方法/触发/注入层级/成本/消融和迁移局限有证据；不宣传 selective 各指标都最好或默认便宜 |
| R068 | 主动提醒接入当前架构 | memory 是 worker-only 服务，能辅助唯一主体及执行孩子，不成为另一条永续人格 | 只读观察授权轨迹、提交 bank/提醒回执；所有额外模型请求走 broker；提醒不反复改稳定 system 前缀；提醒可选择 silent、过期、去重 |
| R069 | 远程 Agent 操作插件 | 能控制 SSH/其他 runtime 上已有 Claude Code、omp、tmux/agent seat | 安装独立 provider；结构化 SDK/RPC 优先、终端为真实兼容路径；不是仅启动一个新 shell 或自家 child |
| R070 | 找回已有远程工作位与进度 | 利用记忆定位原来的任务/session/pane，观察现场后继续 | TaskSeat 与 task lineage、远端会话、host identity 绑定；历史坐标需现场确认；不能把复用 pane ID 当原任务 |
| R071 | 自主回答远端选项和权限问题 | 在已有授权内自行决定，不因远端出现 permission UI 就每次找人 | 回答与 pending question/版本绑定；用户既有授权持续有效；不机械默认 recommended，也不伪造已获权限 |
| R072 | 不确定时主动联系用户 | 按人格、任务情境与可达渠道寻找人类，得到答案后继续原远程任务 | 联系人/渠道记忆、唯一 question ID、跨渠道去重、迟到回答处理；不得只支持在原 Web 页面等待用户返回 |
| R073 | 传统 Skills 与 MCP 市场 | 兼容既有 Skill、MCP 和市场安装/发现生态 | 本地/认证 Git/registry 导入，区分 Skill 文档、脚本、MCP server、hooks 的真实兼容性；市场 manifest 不等于所有功能可运行 |
| R074 | 常用能力更适合原生插件 | SSH/file/memory/远程 Agent 等可有本产品语义工具，减少模型理解成本 | 原生 provider 与 Skill/MCP 互通；不把“支持 MCP”当成无需做好工具体验，也不因此关闭兼容市场 |
| R075 | 插件必须能发现、学会、正确使用 | 安装完成不是产品完成；模型应在自然任务里知道何时调用 | 能力发现、按需说明、例子、前置条件、类型错误与修复；测自然任务发现率、误用率、端到端成功和成本，不只测试 schema 可读 |
| R076 | 认知与插件使用相关论文 | 深研过程知识、工具检索和渐进披露，形成可评估设计 | 主稿采用 SkillsBench、ToolRet、Agent Skills/advanced tool use 等一手研究；具体引用不能冒充用户指定论文或普适效果；选用理由和反例写清 |
| R077 | 关注本地约 27B 模型的能力结构 | 对用户指出的推理/知识/工具负担做具体优化 | 固定模型/量化/模板评测知识、推理、工具选择和长任务状态；带来源知识检索、少参数 typed tools、按需 playbook；不泛化所有 27B 都相同 |
| R078 | 未来模型可替换当前辅助机制 | 今天为弱项补的检索/教学/提示流程不能永久锁死产品 | DiscoveryPolicy、TeachingPolicy、context 策略可替换；未来可 schema-only 或更简单实现；保留稳定语义合同而非固定冗长提示模板 |
| R079 | Python REPL in WASM | Python 本体在 WASM 环境执行，有真实持久交互会话 | CPython WASI/Pyodide 等路线分别声明 ABI、host imports、包范围；native Python 或 SSH Python 另列，不当成 WASM 完成 |
| R080 | Bun REPL in WASM | Bun 是独立具名目标，不能悄悄以普通 JS 解释器替代 | upstream 完整 wasm build 未核验则如实保留差距；Bun 能执行 wasm、bun-wasm 转译器不等于 Bun 本体可在 WASM 跑 |
| R081 | Node REPL in WASM | Node 是独立具名目标，不能用 QuickJS/Javy 兼容部分 API 就勾选完成 | 完整 runtime/模块/内建 API/ABI 支持分别验证；Node 内的 WebAssembly API 不等于 Node runtime 为 WASM target |
| R082 | REPL 可恢复 | cell、会话状态和依赖可跨失败延续，必须说清恢复级别 | 日志/文件/锁、可序列化对象、完整 VM 三层；不能把重新跑脚本当所有变量原地恢复；socket/线程/native handle 与副作用重放另有合同 |
| R083 | REPL 动态装包，全插件 | 各 REPL 可安装依赖、升级和切换环境，解释器不强塞内核 | PackageProvider + 锁定 ABI/generation；区分纯 Python/JS、WASM wheel、native addon/FFI/postinstall；子 Agent 拥有执行席位，主体只委派 |
| R084 | 实时通话/旁听课接口预留 | 可扩展电话/实时语音与持续课堂输入，不是只上传音频文件 | 原生插件扩展点、媒体流/partial-final 修订、打断/播放控制、时间线来源；明确不同 provider 实际支持范围 |
| R085 | 媒体进入同一主循环，能力完整 | 听课/通话时仍是同一主体，可以同时委派所有已有工具工作 | 媒体事件统一进 inbox；搜索/SSH/REPL/记忆等通过孩子执行；不另起自由决策语音人格，也不把通话缩成无工具聊天模式 |
| R086 | 录音/声纹可以自主发展 | Agent 可按已授权能力自己组织录音、识别、转写与开发改进媒体插件 | 子 Agent 可开发/验证/激活新媒体能力；范围/时间线/运行状态真实可见；不要求人类事先手工实现每种未来功能 |
| R087 | 极小核心，一切插件 | WASM engine、SQLite 存储、broker policy、harness、消息/UI/REPL/voice 等业务能力均可替换 | kernel 只保留必要启动/恢复/lifecycle/capability/hard quota 执行原语；bootstrap 插件可换且有健康闭包；轻量测量连同默认常驻插件和 sidecar 合计 |
| R088 | 自己写代码替换任何部分，包括 kernel/rescue | 全系统自我改写已获明确授权；不能把内核或救援代码列为永久豁免区 | 子 Agent 可改写全部源码/协议实现并自主构建、迁移、验证与激活；建议候选 A/B、保留上一代独立可启动恢复闭包；仍满足用户行为合同。不可原地损坏唯一可启动副本，但这不等于永久禁止改 kernel/rescue，也不再询问是否允许自改核心 |
| R089 | Pi 是 for coding agent，本项目是 for personal agent 的最小框架 | 顶层定位是面向 personal agent 的最小可扩展框架，默认 Bot/界面/功能包建立其上；个人主体有跨任务连续性 | 产品定义、核心边界、插件合同与架构图体现个人身份/持续主体/记忆/消息连续性；coding agent 是可启动/恢复/结束的执行 worker 或远程 seat，不替代长期 personal main。此前所有主题须由子 Agent 分工深入研究，并留下来源与研究覆盖 |
| R090 | WASM 只是基础；对获给的 Linux/Windows/macOS 容器、VM、机器进行 Computer Use | 正式支持跨操作系统的桌面/GUI 操作插件；基础 WASM 家园不能成为拒绝扩展到完整环境的边界 | §24 Computer Use 定义 target/session 身份、实际桌面能力、观察与输入、显示/分辨率、断连恢复、取消与来源回执；区分 shell/SSH 与真实 GUI 能力。由执行子 Agent 使用已授予机器能力，主体仍仅委派；Linux/Windows/macOS 各列实现与限制，不默认容器必然有桌面 |
| R091 | 参考 OpenAI 最新 Dynamic Response；按目标界面生成动态 UI | Agent 能根据不同目标界面的实际能力生成交互界面，并在能力不足时降级；通过目标 DSL 的元素/语法说明或 UI 生成工具获知可用能力 | §21 动态 UI 子节定义目标能力发现、可用元素/语法/工具、生成校验及跨界面降级；§23 A25/A26 验证不同能力目标与降级。OpenAI 参考的确切名称、状态与能力由官方专项研究核验，不以用户命名直接当作已发布事实 |

**覆盖优先级更新：**R088 覆盖早期“核心更新另行授权”的假设；R066 覆盖把 research reset/forget 与底层原始事件销毁混用的建议。R052 仍然成立：重写代码的执行者是子 Agent，主 loop 继续只负责语义思考、记忆和委派。R087 与 R088 一起要求“核心小且可替换”，不是“只有插件可改，kernel 永久固定”。

### 对已有需求的解释更新

- R006–R010（自由文档/工具安装/外部记忆）仍是**整体 Agent 系统**的能力；不再默认授予主体 loop。具体执行落到子 Agent。
- R012–R015（恢复、版本、默认 Git memory）继续成立；子 Agent 提出并执行版本和 memory 更新，runtime 提供不可绕过的事务/持久化原语，rescue 不依赖可变子 Agent 正常启动。
- R016–R019（keygen/SSH）由具备权限的子 Agent 执行；主 loop 可以委派建立连接/生成公钥，但不直接运行 ssh/keygen。
- R022、R043（session 真相源/FTS）作为 runtime/记忆子系统基础设施仍需要真实落盘，主 loop 的文件禁令不能被偷换成禁用存储。
- R024 的自治现在进一步明确为持续 proactive single-thread thought loop；仅 cron/被动消息触发已经不足以覆盖。
- R041 的高质量 SSH/file 工具主要暴露给执行子 Agent；主体暴露的是语义 memory 与 handoff 接口。

## 新约束对文件、版本、存储与记忆架构的影响

### 责任矩阵

| 角色 | 可以做什么 | 不应做什么 |
|---|---|---|
| 主体 thought loop | 顺序思考；读取语义记忆/源证据；提出语义 save；委派、评估子任务结果；继续选择下一步 | 任意 path 读写、shell/SSH exec、Git 操作、直接 SQL、直接安装包；不能将 memory.save 变成任意 bytes 写入后门 |
| Memory 子 Agent | 根据语义 save intent 整理/校验/合并事实和文档，更新默认 host-mounted Git memory 或配置的 backend，产出带来源和版本的回执 | 修改 controller 协议、全局能力表、其他人的私密 scope；不能假造已提交回执 |
| Task/progress 子 Agent | 形成任务计划、进度材料、里程碑报告，处理工作产物与 task 事实的一致性 | 拿到其它子 Agent 的 unrestricted workspace；不能被当成全局推理配额唯一信任源 |
| Version/self-improvement 子 Agent | 在独立 branch/workspace/generation 修改任何部分，包括 Git/包/插件/指令、kernel/rescue；构建、迁移、验证并按既有授权激活候选 | 原地破坏活跃实例或唯一可启动恢复副本；违反用户行为合同；污染所有并行子任务共享根。不是禁止替换 kernel/rescue 实现 |
| Runtime/control plane | 持久记录收到的事件、handoff intent、执行状态、lease、回执与 outbox；执行工具权限、资源上限、原子激活；提供确定性 memory index/read 适配 | 作为另一个隐藏自由行动的“主 loop”；绕开用户限制给主体直接文件工具 |
| Rescue/supervisor | 独立检查活性、验证启动 generation、杀死失控 worker、恢复 LKG/factory、重放已提交任务/会话状态 | 依赖损坏沙箱里的脚本/人格/子 Agent 才能恢复；回滚身份撤销、吞掉已接收消息或重复外部副作用 |

上表为**与新增硬约束一致的架构建议**，不是用户已批准的具体组件拆分。尤其 task/progress worker 是否专用常驻身份，仍可为按需角色。

### 语义 memory API：允许主体 save，但实际写由子 Agent 完成

推荐的数据流（规格草案，非实现）：

1. 主体发出 `memory.save` 语义意图：namespace、事实/计划/观察、来源 message/span、置信/有效期、关联 task，以及一个幂等键。不得接受任意宿主路径、裸 SQL、脚本、二进制写入或权限扩张参数。
2. Runtime 将意图作为 durable event 记录，返回 `accepted`/`pending` 和 request ID。**accepted 不等于已更新 canonical memory**。
3. Memory 子 Agent 消费意图，读取授权范围内的证据，在其 branch/workspace 内更新文件/数据库后端，处理相冲突事实，提交并形成 receipt。
4. Runtime 记录提交回执（memory version / Git commit / source refs）。主体通过后续事件或 `memory.read` 得知该 save 已 committed/failed/conflicted。
5. 如果 `memory.save` 采用等待完成式返回，它须等待 memory 子 Agent 回执，而非偷偷由主 loop 直接写文件；等待期间不得持有 LLM request permit。

为了实现 read-your-writes 而不伪造已提交事实，可提供明确标记的 pending intent overlay；查询可见 `committed_memory` 与 `pending_intents` 两种状态。主 loop 重启后也能看到尚未完成的 save。默认 host-mounted Git memory（R015）作为 canonical 文件记忆仍保留，controller/event ledger 不是它的替代品。

`memory.read/query` 可以由确定性索引服务直接返回语义记录或来源片段，这不构成赋予主 loop 任意文件权限；需要 memory 推理/整理/修复/导入时委派 memory 子 Agent。若用户要求所有 memory read 也必须经子 Agent，可以切换，但这不是现有摘要中已确认的额外约束。

### Runtime 落盘不等于主 loop 获得文件能力

用户限制的是主体 Agent 的行动能力，不是整个程序的持久化职责。运行时仍必须自动写入：

- 原始会话、收件事件、主循环的可观察决策/委派事件及工具调用结果。
- pending handoffs、child lifecycle、外部操作 receipt、outbox 和重试幂等键。
- 模型 admission queue/lease epoch 所需恢复状态（具体哪些需要 durable、哪些可重建由实现选择）。
- memory save intent 与 completion receipts；package generation 激活记录与 crash evidence。

“可观察决策”指任务选择、目标、简洁行动理由与结果，不要求或假设模型提供完整隐式思维链。不能为了“持续思考”凭空声称可保存/回放不可访问的模型内部状态。

这些数据是 runtime 的 append/transaction 行为；主 loop 无文件句柄、无任意路径、无 SQLite writer 能力。否则禁止主 loop 直接文件操作会错误地变成“controller 崩溃就失忆、派发任务无记录”，违背 R003/R012/R022/R043。

### Branch/workspace scope 绑定子 Agent

- Workspace/branch 必须标注 child instance、task、memory namespace、capability scope、base generation、parent task；继承的是授权上限，不是所有 sibling 的访问权。
- 主体 loop 本身可以无可写 workspace；其 prompt/context/checkpoint 由 runtime 管理，不提供 shell cwd。
- 默认 memory Git mount 可给 memory worker 专用；其他执行子 Agent 对其直接写权限应受角色/任务限制，避免多个 branch 无协调修改同一 live Git worktree。
- 如需要多个 memory worker 并行，使用独立 worktree/COW branch + serialized publish/版本检查，不能靠一个全局 Git 工作目录同时 `checkout`。
- workspace fork 不自动复制 SSH 私钥、凭据 vault、controller ledger、管理员能力或子任务外部副作用；这些通过引用/能力绑定提供。
- 对 direct RW host mounts 与 SSH remote writes，仍适用原先不可由 FS rollback 撤销的边界；子 Agent scope 不是虚假的分布式事务。

### 自我改进与 rescue 的边界

自我改进任务可产出**任何部分**的候选代码/插件/人格/包/kernel/rescue generation；独立验证子 Agent 或确定性验证门检查后，按既有授权自动构建、迁移和激活。失败保留证据并回退。主 loop 可以委派改进/审查、选择是否继续探索，但不能因“允许自改”获得直接文件/exec 工具。这里约束的是执行路径与用户行为合同，不是永久冻结任何一部分源码。

推荐用候选 A/B 与代际恢复协议：候选运行前保留上一代**独立可启动**的控制程序、最小引导元数据、必需插件闭包与数据兼容恢复路径；健康条件成立后切换，失败自动退回。rescue 的实现同样可升级/重写，不能再写成永久不可变模块。恢复不依赖正在被替换的新 Agent、坏插件图、人格或模型 API。保留的上一代在候选验证期间只读，是一次升级过程的恢复条件，不是永远禁止 Agent 改 rescue。**“单线程主体”不等于“没有独立恢复机制”，也不等于“整个服务只能一个 OS 线程”。**

数据库/凭据/事件 schema 迁移是同一协议的一部分：只保留旧可执行文件但把唯一持久状态不可逆地迁到旧程序读不了的格式，不算可恢复。建议兼容迁移、候选副本验证或独立可恢复状态版本；不要让代码回滚复活已撤销权限或擦掉已收消息。这些是待工程验证的恢复设计，不能声称已证明。

### 持续思考与全局推理资源的结合

- 主 loop 的决策 turn 顺序执行；模型请求仍按全局 scheduler admission 进入，不能私自占用无限 slot。
- handoff 完成即释放该次 LLM permit；子 Agent 获得独立请求排队权，主 loop 可继续选下一步而不是 hold-and-wait。
- 队列满/模型离线时，UI 要显示真实 `waiting_for_model`/`recovering`，不能表演持续生成的“思考动画”。
- 避免把主 loop 永远置于最高优先级而饿死 memory 子 Agent：否则 save 永远 pending，主 loop 不断重复思考。保持单线程只约束 controller stream，不需要排除子 Agent 的并发推理。
- 不能未经用户同意加入“无任务自动休眠停止思考”的产品默认值；若要节能 backoff，应作为额外规格追问。持续性需求与物理不可用期间的恢复语义应分别描述。

## 新约束的反弱化检查

1. `memory.save` 是否被偷换成主 loop 能给任意 path 写内容？若有，是绕过 R052。
2. 是否因 R052 删除 controller ledger/事件持久化？若有，错误地把 Agent 能力限制扩展成 runtime 无法保存状态。
3. 是否仍把 SSH/file/exec/git tools 注入主体工具列表，再靠 system 写“别用”？若有，缺少真实能力隔离。
4. 是否把 single-thread 定义成禁止所有 runtime I/O worker 或子 Agent 并发？若有，超出了用户要求并损害原本并发研究目标。
5. 是否让主 loop 持有唯一推理 permit 等 memory/child 返回？若有，在 cap=1 下会形成死锁。
6. 是否把 runtime 的 task-event 记录当成违反“任务进度发生于 subagent”，从而不保存 child started/completed？若有，混淆 task execution record 和 Agent 自主编写进度材料。
7. 是否将人格以同级自由文本覆盖工具协议或 capabilities？若有，违反 R054；persona 只能被当作有边界的风格/性格输入。
8. 是否让 self-improvement worker 原地破坏唯一可启动 recovery generation？若有，不能满足无人恢复。反向也应检查：是否因此把 kernel/rescue 源码永远锁死？若有，违反 R088。
9. 是否把主 loop 的手写反思日志当作必须收集的完整思维链？若有，规格假设了模型未必暴露的内部数据，应改成显式决策事件/简要理由/证据。
10. 是否为了省资源把持续思考默默改成被动唤醒？若有，必须退回待裁决，不得当默认实现。

## 最容易被建议悄悄弱化的条款

### 1. 把 ssh wasm 换成宿主 SSH

- 关联：R018、R019、R041。
- 风险：表面上都提供 `ssh.exec`，但用户强调的是 WASM 环境中的 SSH 能力。
- 正确写法：`真实 WASM SSH 客户端：用户要求，待 port 验收；Rust host SSH transport：建议的可选补充，未获用户确认。`
- 不可写：`已决定 v1 使用 native SSH，满足 ssh wasm。`

### 2. 把默认宿主挂载 Git memory 改成 SQLite-only

- 关联：R015、R022、R043。
- 风险：会话事件用 SQLite 合理，不代表可删用户指定的文件化 Git memory。
- 正确写法：默认 `/memory` 来自宿主目录并初始化 Git；SQLite 保存原始会话/索引，两者职责、引用和一致性有协议。
- 不可写：`为保持核心简单，暂不提供 Git memory。`

### 3. “任意 mount”变成固定 `/workspace` sandbox

- 关联：R005、R013。
- 风险：为了回滚或隔离便利，仅开放一个固定目录；这是实质降级。
- 正确写法：可配置所有宿主账号可访问路径，按 mount 显式选择 direct/COW/RO；不能快照的外部状态不冒充可回滚。
- 需要裁决的是 agent 是否能自行新增宿主 mount，而非是否支持任意路径映射本身。

### 4. “永远自主”变成 cron + 等人发消息

- 关联：R024、R037。
- 风险：有定时器不等于拥有持续目标、任务发现、自维护和无人推进。
- 正确写法：定义常驻 lifecycle、主动维护、目标队列、探索/推进、资源预算和睡眠/恢复；预算是实现自治的资源约束，而非取消自治。
- 所有具体 idle tick、夜间预算、自动升级权限均未确认。

### 5. “Linux/POSIX 任意工具”变成少数 Rust 内置命令

- 关联：R004、R007、R008、R010。
- 风险：高质量 fake shell 能减少推理开销，但不能完成真实可安装程序环境的需求。
- 正确写法：保留真实工具执行层；明确兼容 profile 和不兼容时的待确认 backend；文件工具只是额外捷径。

### 6. 把 GCC 悄悄替换为 Clang

- 关联：R008。
- 风险：用户“例如 gcc-wasm”可能表达完整编译工具链目标，也可能点名已有项目；当前无答案不能断定可替代。
- 正确写法：研究区分 hosted compiler 与 compiler target；Clang 是有证据候选；GCC 条目保留为待用户确认含义/待技术验证。

### 7. 有 gh.wasm 就宣称私有 Git 认证完成

- 关联：R014、R015。
- 风险：已发现社区 gh WASM 的 HTTP/exec 需要专有 host module，private Git credential helper 链路另有限制。
- 正确写法：private clone/pull/push 是端到端验收；gh/tea port、凭据与 Git transport 分开记录。

### 8. Core 用 Rust，但完整 Agent 实际依赖常驻 Node/browser

- 关联：R011、R021、R029、R044。
- 风险：Rust bootstrap 很小，却让默认完整产品强制依赖几百 MB 的另一层。
- 正确写法：列清常驻所有进程和合计资源；非 Rust 可选 profile 明示，不把开销藏在 sidecar。

### 9. “最新主线 bump”变成永远锁一个 RC

- 关联：R032、R033。
- 风险：可复现性需要 pin，但 pin 不等于可忽略用户持续更新指令。
- 正确写法：每个触发周期先核验主线、bump/pin 到具体 SHA、进行迁移回归、记录失败。精确“每次”的定义尚待用户确认；不能声称其接受只按稳定版升级。
- compiler adapter 不能只出现在非目标清单，应有可评审接口边界。

### 10. 双语教程变成一句“兼容 OpenAI API”

- 关联：R039、R040。
- 风险：Ninfer、Strata 的 context、parallel、KV 接口差异被忽略，两个具名生态没有真正文档。
- 正确写法：两个项目 × 中英两种语言的覆盖矩阵；核验项目身份后分别提供配置、调度、slot 能力、运行限制与排障。未实现产品的命令不能假装已可执行。

### 11. 默认对话开放变成默认 pairing/群白名单

- 关联：R001、R035、R036。
- 风险：照抄 OpenClaw 等已有安全默认值，直接违背用户“不限制太多”的明确取舍。
- 正确写法：开放对话与管理/机器能力不同；默认任何平台可到达的用户能对话，管理员手动指定。若有资源防刷，须说明为可配置服务保护且不能暗改身份门槛。

### 12. Slot passthrough 变成普通 prefix caching

- 关联：R027、R028、R038、R039。
- 风险：slot lease、backend affinity、exact prefix、请求调度是不同能力；统一 OpenAI API 可能将扩展字段丢掉。
- 正确写法：provider extension mechanism、能力检测、可选 slot passthrough 与降级行为清楚；无该能力的后端照样工作，但不得称其满足 slot 复用。

### 13. 完整 session truth 变成不断覆盖的摘要

- 关联：R022、R043。
- 风险：compact 后只剩 summary，不能回看原话、工具结果和失败证据。
- 正确写法：raw session source 与 FTS/summary/embedding 分离；任何摘要都有 source span；检索能展开原记录。

### 14. Android 部署变成浏览器访问桌面端

- 关联：R044。
- 风险：手机访问 Web UI 不等于手机运行内核、沙箱和记忆。
- 正确写法：Android local node 单列 deployment profile；后台可用性根据 native APK/Termux/常供电场景说明，无法永久常驻的现实约束不抹去部署目标。

### 15. 只追新技术，删掉稳定和恢复要求

- 关联：R002、R003、R011、R012、R013、R021、R044。
- 风险：LayerFS/新 runtime 一旦 README 宣称高性能就变成强依赖，但核心数据没有 crash-durability。
- 正确写法：允许最新方案进入候选，有独立持久性/恢复门槛；用户提出“稳定”是目标，不是已经成立的事实。

## 当前全部仍未确认的团队推荐

以下建议可以进入草案，但必须有“未确认”标记：

| 未确认假设 | 影响条款 | 不得假装用户已同意的内容 |
|---|---|---|
| 单主人 + 多会话隔离为首版模型 | R001、R026、R036 | 用户尚未排除多主人/多租户 |
| 允许可选 Rust host SSH/Git transport | R014、R018、R019、R041 | 不能据此删掉真实 WASM CLI 需求 |
| Linux ARM64/x86_64、512 MiB/1 GiB 级参考设备优先 | R021、R044 | 用户尚未认可设备下限、排除 MIPS 或推迟 Android |
| Wasmer/WASIX 为 rich userland 候选；Wasmtime 为标准能力候选 | R004、R008、R029 | 运行时尚未冻结；不能称已确定生产架构 |
| 不兼容 memory 系统走 SSH/native container | R010、R018 | 用户只提后续可支持容器，首版降级路线仍待裁决 |
| Nix-like runtime + 原生 Nix 构建端 | R008、R009、R020 | 用户未确认不需要 guest 内 Nix 本体或 self-hosted build |
| 先回滚 last-known-good，再回到 factory | R012 | 推荐恢复顺序合理，但用户只明确自动 factory fallback；应保留其原要求 |
| runtime 回滚保留 memory/session/identity/撤销记录 | R003、R012、R015、R016、R022 | 这是推荐一致性语义，须写清并征求裁决，不能隐含 |
| 默认挂载使用 COW，direct RW 为可选 | R005 | 用户未选默认挂载语义，不能限制其“任意路径”能力 |
| 主动任务的默认预算、频率、停止条件 | R024、R027、R028、R037 | 用户未确认具体额度；不能用极低预算削弱“全自主” |
| 候选 A/B 的验证阈值、健康窗口、保留代数和兼容迁移实现 | R009、R011、R012、R024、R088 | 用户已允许任何部分自改；未确认的是具体工程参数，不能退回“内核是否可改”重新求授权 |
| 所有具体性能阈值、时延/RSS/RPO/RTO | R012、R021、R024、R027、R037、R039、R044 | 本轮没有实测，数字必须是待验证目标 |

## 整合完整 SPEC 时的最终勾检

1. 所有 R-ID 是否都有正文落点，而非只列在附录？
2. 每项技术不可行/未验证处是否保留原需求、差距和待裁决问题？
3. 每条推荐是否区分用户授权、设计推导与未经回答的假设？
4. 具名参考是否全部出现：AstrBot、LayerFS、Dot、Duet、Muse、DeepSeek Harness、Proto-UI、shadcn、iLinkBot、QQ 官方 API、NapCat、napcat-cli、飞书、OpenClaw、Ninfer、Strata、oh-my-pi、pi、cordis-rs、SQLite FTS5、Nix？
5. 文件化 Git memory 与 SQLite session truth 是否同时成立？
6. 默认开放对话与手动管理员配置是否明确，且没有暗加 pairing 门槛？
7. SSH WASM、gh/tea WASM、任意挂载、Android local runtime 是否标注真实能力状态，而不是被接口名称遮住实现差距？
8. 主线 bump 和 compiler seam 是否在工程要求正文，不只是口头承诺？
9. Ninfer/Strata 双语教程是否有明确四格交付及版本验证计划？
10. 用户未回答追问时，标题是否正确写为“完整草案/待裁决”，而非“已确认产品规格”？
11. 当前阶段是否保持研究/规格，无实现、无悄悄创建项目、无未经指示发布？
12. R055–R091 是否逐项落地，且没有把同一主 loop 的消息/媒体输入拆成多个自由主体？
13. 半 KV 池是否写明另一半是所有 children 集体使用，C=1 与多份 KV 驻留是否分开？
14. 语义 forget/research reset 是否保留原始历史，主动 memory 是否有 silent 与全局资源计费？
15. 远程 Agent 是否可以在既有授权内自主回答选项/许可，而非每次都交给人类？
16. Python、Bun、Node WASM 是否各有独立真实状态，不用“类”或泛 JS 偷换具名目标？
17. 插件发现/教学/正确使用是否有任务级验收，传统 Skills/MCP 是否继续可用？
18. 所有核心与救援源码是否均允许由子 Agent 改写，独立恢复是否通过代际产物而非永久锁源码实现？
19. 顶层定位是否明确为 personal agent 的最小框架，而非 Pi 的 coding wrapper 或必装全部渠道的单体应用？
20. Linux/Windows/macOS Computer Use 是否是独立具名插件目标，且没有用 SSH 命令行或 WASM 能力冒充桌面操作？
21. 动态 UI 是否能发现目标 DSL/元素/工具能力并生成或降级，而非只在一个固定 Web 界面渲染预设组件？

## 本轮主稿覆盖映射与待同步项

审查对象为 `agent_bot_spec_v0_1.md` 的当前草案。父 Agent 正在同步修订；以下是本次读取时的观察，不代表最终文件仍存在相同问题，也未修改主稿。

| 需求范围 | 已有主稿落点 | 覆盖状态 |
|---|---|---|
| R055–R060 消息/API/认识人/声纹 | §2.15、§7、§15 | 主体路径与身份语义已有；§7 的统一 provider 清单可补联系人/媒体等，避免后文接口成为未登记旁支 |
| R061–R064 C=1、半池、自动/主体调控 | §2.16、§13、`kv_policy_spec.md` | 主要语义已有；主体策略调整须落为语义请求/委派，不新增万能执行工具 |
| R065–R068 两论文、reset、保留、PMA | §14，历史合同 §12 | 论文限定与 worker-only 设计已有；§12.4 的真实删除流程要与 reset/semantic forget 明确分开，不能把任何一者当另一者 |
| R069–R074 远程 seat 自主、Skills/MCP、原生插件 | §17–§18、`remote_agent_spec.md` | 主要覆盖；保留自主决定和跨渠道找人，不恢复每个 permission 必问的旧默认 |
| R075–R078 插件认知、本地模型、未来替换 | §18 | 有自然任务/成本评估；论文与厂商工程参考的证据层级仍应区分 |
| R079–R083 Python/Bun/Node、恢复、动态装包 | §19 | 分目标能力表已有；§2.20 的“Python/Bun/Node 类”宜删除“类”，避免把兼容部分 JS 当 Bun/Node 完成 |
| R084–R086 实时通话/课堂/自主媒体演进 | §20 | 主要覆盖；同一主 loop 和完整委派工具能力已明确 |
| R087 极小核心、全部业务插件 | §7、§22 | 核心/默认插件合算已写；建议统一接口目录补齐 SessionStore/BrokerPolicy/ContactProvider/RemoteAgentProvider/REPL/媒体，具体命名未冻结 |
| R088 任何部分自改 | §9 有候选和 A/B，但读到版本尚未明确 kernel/rescue 也可自主替换 | 必须补入 §2/§7/§9/§23；单独写“可替换 harness”不足以覆盖这一最新更正 |
| R089 personal agent 最小框架 | §1/§7 尚以一个 Agent 产品描述为主 | 补顶层定位及“最小框架 + 默认个人 Agent 发行/插件配置”的关系；区分长期主体与临时 coding workers |
| R090 Computer Use | §24（父 Agent 正在整合），§23 验收 | 基础 WASM 与可选完整 OS/GUI provider 分开；三大桌面 OS、被给予的容器/VM/机器和子 Agent 执行合同须保留 |
| R091 动态 UI | §21 动态 UI 子节、§23 A25/A26 | 按目标界面能力生成与降级；DSL/可用元素说明及 UI 工具发现；官方参考资料由专项研究提供 |

### 需要优先消除的歧义或遗漏

1. **自改范围不能被白名单缩小。**读取时 §9 只列工具、skills、memory 策略、提示词、可替换 harness；应明确是举例而非允许范围全集，kernel/rescue/生命周期/存储/provider 均可重写。§9 的“不能直接覆盖运行中 supervisor”可以保留为不得原地破坏唯一活跃实例，不可理解为永远不能替换。
2. **合同保持与代码冻结分开。**唯一主体、主体无直接文件工具、人格不改协议、既有资源配置、来源完整性是用户行为合同；可由任何新实现继续满足。不应以“由宿主保证”为由把宿主实现变成永久不可改。人格/提示词自改若仍需额外裁决，需说明具体未决行为而非重新否定全系统自改授权。
3. **新增全系统升级故障验收。**现有 A18 主要验证业务插件坏了 core rescue 仍在；需额外场景验证新 kernel/rescue 自身坏了可退上一代、迁移后旧版本仍能恢复、事件不丢失、无双 main。保留旧 binary 但 schema 已不可逆升级不算通过。
4. **遗忘四个意图分开。**research reset、semantic forget、memory suppression/retention、owner 真实 erase 不应合为一个按钮或一个 API。§12.4 可保留 owner 删除设计，但不要以真实删除问题未决为由重新询问 research reset 是否删除历史；后者已经明确不删。
5. **最小框架定位须落在默认组成。**默认插件满足个人 Agent 的持续身份/记忆/消息/能力委派；coding seat、NapCat、语音、REPL、浏览器等可选能力不能都成为内核启动前提。统计体积和常驻成本必须包括默认插件，不只报 kernel。
6. **后续附录同步。**`spec_questions.md` Q5 应明确讨论 owner 真实删除，标出研究 reset/semantic forget 不在待裁决范围；`thought_loop_spec.md` §7.2 的“owner 授权可允许自动 promotion”应承认当前已授权范围。早期研究笔记中的永久不可改 rescue 建议均由 R088 覆盖，不用于最终规范。

本轮未发现需要删除任何用户需求的理由。WASM Node/Bun、真实 SSH WASM、完整 Linux/POSIX 兼容、Android/router 与硬 50/50 KV 的技术状态仍须按具体后端验证；这些差距应在完整目标中保留，不使用“最小框架”定位削掉它们。

**后续同步说明：**父 Agent 已确认主稿 §1/§2/§6/§7/§9/§12.4 修正了上文记录的定位、自改/人格审批和遗忘操作歧义。上述缺口记录用于审查追踪，不表示仍未修复。最终精简落点采用 §24 Computer Use、§25 NInfer、§26 Strata、§27 中英排障、§28 待决事项、§29 路线。
