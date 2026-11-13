# Shenko（深刻）

Shenko 是面向 personal agent 的最小框架：一条持续存在的主思考流、一个持久 WASM 家园，以及一组可安装、可替换、可自我演进的能力。编码、聊天、研究、远程控制和电脑操作不是它的分区功能，而是同一主体按需安装的能力包。

源码采用 Rust 2024 workspace 与 cordis-rs 插件主干。CLI 与 Android 宿主共享 `bin/shenko/src/lib.rs` 的运行时装配，`bin/shenko/src/main.rs` 只承担 CLI 入口。功能合同见 [文档索引](docs/INDEX.md)，实际运行的候选、结果与限制见 [证据目录](docs/evidence/README.md)；源码存在不等于运行验收。

## 产品是什么

- **一个持续的主体**：唯一有效的决策序列在一条分支上推进。children 是有限任务执行者，可以有自己的 system prompt、模型、上下文与工具预算，也可以继续委派与双向讨论。
- **可安装的能力**：perception、awareness、Skill 与 code 共享同一安装单元权威，按需发现、按需读取精确版本正文。核心不做关键词判断，也不把"所有正文灌进上下文"当作唯一方案。
- **真实的事实与效果**：原始 session、持久任务、效果账本、未知态与可恢复迁移是产品的一部分，不是一次性生成结果。
- **可替换的实现**：内核、恢复器、provider、渠道、设备适配器都能被替换并在交接后继续工作；核心只保证机制，不保证某条渠道或某个模型。
- **多个节点，同一个自我**：节点各自持有真实能力与凭据。连通域内有一个主节点，分区后递归选择实际可用的备用节点。

## 安装

仓库需要 Rust 1.93 或更高版本与 Cargo，使用 Rust 2024 edition。1.93 是当前锁定的 Wasmtime/Wiggle 依赖声明的最低工具链要求；具体已验证工具链以运行证据为准。

```bash
git clone https://github.com/cyjin-yl/shenko
cd shenko
cargo build --release -p shenko
```

依赖包含一个固定 revision 的 git 依赖 `cordis-core`（`rev 9a8a49fa459991ee29da73fb42ea9324cee9b680`，MIT）。首次构建需要能取到该 git 依赖与 crates.io 索引；离线环境按 `.agents/skills/shenko-dev/SKILL.md` 的说明预取依赖。

产物是单个可执行文件 `target/release/shenko`：

```bash
./target/release/shenko check     # 校验数据目录、Store 与引导视图
./target/release/shenko version
```

数据目录默认取用户数据目录下的 `shenko/`，可用 `--data-dir` 或 `SHENKO_DATA_DIR` 覆盖。首次 `shenko start` 会在该目录建立 SQLite WAL 库、CAS 对象目录和 WASM 家园。

## 使用

```bash
shenko start --port 7860            # 启动主体、插件装配与 Web 时间线
shenko identity --history           # 查看实例的名称、灵魂与人格修订历史
shenko runtime status <id>          # 查询一次主体边界请求的持久结果
shenko sync status                  # 本地持久事实、未确认变更与 peer watermark
shenko tool list                    # 已安装 WASM 工具与具名目标的真实状态
```

模型绑定通过环境变量，不写入代码：

| 变量 | 作用 |
| --- | --- |
| `SHENKO_USE_LOCAL_MODEL=1` | 使用本地 OpenAI 兼容端点 |
| `SHENKO_LOCAL_MODEL_URL` / `SHENKO_LOCAL_MODEL` | 端点与模型名 |
| `ANTHROPIC_API_KEY` | 使用 Anthropic provider |
| `NAPCAT_WS_URL` / `NAPCAT_BOT_QQ` | QQ 通道接入与账号绑定 |
| `SHENKO_QUOTA_POLICY` | 一个序列化额度策略文件；读取失败即启动失败，不静默按无额度运行 |
| `SHENKO_DATA_DIR` / `SHENKO_LOG` | 数据目录与日志级别 |

没有配置模型时，`shenko start` 仍然启动确定性恢复服务与 Web 界面，并明确报告"模型容量 blocked"，不会假装推理可用。

Web 界面默认在 `http://127.0.0.1:<port>/`，提供同一条主体时间线：`/ws` 是输入与输出通道，`/api/tasks`、`/api/memory`、`/api/config`、`/api/self`、`/api/context` 读取当前事实，`/playground` 是持久 Playground。

## 扩展

能力通过插件进入运行时，运行时装配在 `bin/shenko/src/lib.rs` 中可见。扩展只有一个入口：插件经同一个生命周期进入，向能力目录登记它能做什么，向钩子图登记它在什么时刻被调用。哪些规则属于内核、哪些是可以替换的参考插件，见[内核合同](docs/specs/kernel.md)；当前实现与它的差距见[内核收敛计划](docs/development/kernel-convergence.md)。

- **原生任务处理**：实现 `NativeTaskHandler`（`descriptor` / `execute` / `reconcile`）并用 `NativeTaskHandlerPlugin` 注册，即可成为主体可委派的能力。已有的例子是 WASI 工具执行、Computer Use、Playground 后端、错误读取、issue 草稿与自解释。
- **provider**：实现 broker 的 provider 合同接入推理；额度观测与 admission 是独立策略，不写死模型名单。
- **渠道插件**：插件同时拥有入站分类、来源/账户命名空间与出站描述、派发与回执。目标标识是不透明字符串。
- **意识与感知单元**：`shenko-awareness-core` 解析 awareness 文档，`shenko-node` 的单位库保存版本、依赖、启停与试用结果。纯文本单元不启动 daemon。文档是当前的 awareness harness，不是能力本身；概念合同见 [51 认知元概念：内容、态度与理由](docs/specs/cognition-units.md#spec-51)。
- **WASM 工具**：`shenko tool install --file <artifact> --name <n> --version <v> --abi wasi_preview1` 安装真实二进制，`shenko tool exec --tool <name>` 在受管 mount 下执行。声明了 ABI 但没有可核验工件的目标会被如实标记为未解析。

- **第三方生态适配**：`compat.mcp` 与 `compat.skill` 是两个**独立、可选**的插件。用 `shenko compat mcp import <file> --unit-id <id>` 记录一个显式配置的 MCP 端点，或用 `shenko compat skill import <dir> --unit-id <id>` 原样导入一份 Agent Skill 材料，然后 `shenko compat enable <unit-id>` 才会向主体提供。产品不内置任何厂商 server、厂商 Skill、账号流程或依赖安装；第三方实现保持独立可移除，说明见 [docs/specs/ecosystem-adapters.md](docs/specs/ecosystem-adapters.md)。

开发入口与协作规则见 [AGENTS.md](AGENTS.md)（`CLAUDE.md` 是指向它的符号链接）与 [shenko-dev Skill](.agents/skills/shenko-dev/SKILL.md)。

拉取请求的六项原生 CI 门禁定义在 [ci.yml](.github/workflows/ci.yml)。容器中的 Git 步骤在 checkout 后为当前 `GITHUB_WORKSPACE` 注册精确的 `safe.directory`；checkout 在临时 `HOME` 下写入的配置不会自动保留到后续 shell。Clippy 与内核边界检查继续使用独立的不可变基线，既有警告不作为新增警告的预算。

## 文档导航

从 [INDEX.md](docs/INDEX.md) 开始，按问题进入具体合同及其 Why。根 SPEC/WHY 不再是正文来源；引用行为时直接链接领域文档的相应段落。历史原文与用户来源仍保留，迁移不缩减产品范围，也不代表新增运行验收。

| 文档 | 内容 |
| --- | --- |
| [领域规格索引](docs/INDEX.md) | 旧规格入口，仅导航；现行正文已按领域拆入 docs/ |
| [docs/specs/kernel.md](docs/specs/kernel.md) | 内核合同：九条不变量、每条的 Why，以及参考插件的边界。先读这一份 |
| [docs/specs/architecture.md](docs/specs/architecture.md) | 当前功能边界与各层事实归属 |
| [docs/INDEX.md](docs/INDEX.md) | 全部文档的真实盘点、阅读顺序与归档去向 |
| [docs/specs/](docs/specs) | 内核合同与 30 份领域功能合同，按领域阅读功能、接口、Why、错误与恢复 |
| [docs/governance/](docs/governance) | 自主决定边界、完整交付合同、许可与接续 |
| [docs/requirements/](docs/requirements) | 165 项 R、63 组 B、59 条来源与 236 条验收的机器可读索引，每条绑定实际来源文档 |
| [docs/evidence/](docs/evidence) | 已保存的真实运行证据与其边界 |
| [docs/state/coverage.json](docs/state/coverage.json) | 每项需求的实现与证据状态 |
| [docs/history/](docs/history) | 历史来源：原规格快照、46 份旧稿、已归档的任务入口与任务卡 |
| [设计理由索引](docs/INDEX.md) | 旧理由入口，仅导航；Why 与对应领域合同一起维护 |
| [迁移映射](docs/contract-migration.json) | 旧章节到当前文档与锚点的完整去向、原件哈希与迁移块 |
| [CONTRIBUTING.md](CONTRIBUTING.md) | 贡献与协作约定 |
| [CONTRIBUTORS.md](CONTRIBUTORS.md) | 贡献来源与署名归属 |

历史任务入口（`GOAL.md`、`PROMPT.md`、`HANDOFF.md`）、旧进度板与 22 张社区任务卡已按 [§48.5](docs/specs/architecture.md#spec-48-5) 移入 `docs/history/`，原件字节保留，不保留根目录兼容入口。

## 许可与归属

社区版采用 AGPL-3.0-or-later（`LICENSE` 与 [docs/governance/LICENSING.md](docs/governance/LICENSING.md)）；owner `cyjin-yl` 本人保留闭源与商业双许可，贡献者再许可条款见 [CONTRIBUTOR_LICENSE.md](docs/governance/CONTRIBUTOR_LICENSE.md)，来源声明见 [DCO.md](docs/governance/DCO.md)。第三方依赖各自许可保留，清单见 [LICENSE-INVENTORY.md](docs/governance/LICENSE-INVENTORY.md)。
