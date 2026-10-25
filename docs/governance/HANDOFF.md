# Shenko 开发交接状态

**更新时间：2026-10-26（C20 正式 S0／持久窗口已实测；完整 GOAL 继续）**

## 当前接续：C20 正式 loop／恢复

- 当前候选 base `1c51c7b`：生产 S0 已接原始输入暂存、持久 HistorySource／WindowLog／ContextPolicy。冻结 schema 8 和最终 Store CAS 绑定原始输入、raw leaf、profile／模型代际、source commit seq 与能力／预算来源；同 profile 重启保留窗口，旧实例缺少原始结果事实时明确阻断迁移。状态变化的调用单元重新水合，不从旧摘要推断当前成功。
- 最终受影响回归 **42 passed／0 failed／exit 0**，命令为 `cargo test --offline --locked -p shenko-types -p shenko-broker -p shenko-store -p shenko-context-policy -p shenko-compact-budget -p shenko-compact-snap -p shenko-subject -p shenko --lib --tests -j1`。覆盖重放、恢复、竞争、当前预算、旧历史准入、真实 memory 回执和状态重水合；不是全 workspace 或 CI 绿灯。
- 真实运行：预封存输入经 SIGKILL 后恢复，真实模型提交并进入 WS；模型首次运行与无模型重开均 exit 0，input 439／output 264。随后同目录模型重启 exit 0，后继 `Preparing` 的整份冻结输入一致，同一窗口/profile/`model_generation=1` 保留，上一轮实际输入 439 被恢复。真实 memory intent 的同一 call ID 依次产生 `tool_call`／`tool_result`，重开读到 `memory_receipt outcome=committed`。证据 `shenko-dev-live:/tmp/shenko-context-runtime-yztx927b/{result.json,model-restart-result.json,first-inspect.json,reopen-inspect.json}`；真实元数据证据 `/tmp/shenko-context-cap-consumer-8c1f/metadata-result.json`（广告上限 262144，视觉事实未知）。旧二进制 schema 6 的拒绝证据另存，不冒充本链路通过。
- 下一步是 C09 历史检索／重读、旧实例显式迁移、真实 KV/prefill 和包闭包；真实会话未触发 compact，不声称真实 Snap／视觉额度／物理 KV 已验收。完整 C20、#9／#25 与 S1–S5 保持开放。

## 已交付基线：C04 原生多模态、C05/C11

- 当前已合主线包含 PR #55（有限 native child）、#57（并发公平 broker）、#62（持久历史/窗口）、#64（AGPL 采用）、#65（真实 Proto-UI）、#63（C05，merge `3e3ca0a832`）与 #66（原生图片 wire，merge `1c51c7b72d`）。这些是局部交付，不是完整 S1–S5 验收。
- 新增 C04 原生 Text/Image parts：两个 provider revision 3、冻结 S0／Store identity admission 7；旧协议阻断，不留字符串兼容分支。现有 broker 2 项与修复 Store 版本校验后的 store/subject/bin 24 项测试通过。两个真实 Rust consumer 经 payload 持久化重读后，分别走 OpenAI-compatible／Anthropic-protocol HTTP，均读出同一 PNG 的 `271828`；无效 MIME／空图片在发出前拒绝并释放容量。首轮 Anthropic 1024 上限空文本失败仍保留；新 caller 先存完整响应、4096 上限请求通过。精确源输入、证据 locator 与非 Claude／KV／四流验收边界见 [C04](../tasks/C04.md)。原生 wire 不等于正式投影；本候选的接续见上，#9／#25 均不关闭。
- C05 交付入口是 [PR #63](https://github.com/cyjin-yl/shenko/pull/63)。整合产品候选 `2a82a8e` 保留原 C05 提交，正常合并上述 main；已解决 API 冲突、全部 4 参 bootstrap 调用和无模型 native child 恢复。没有模型时仍服务配置/UI并处理确定性 child，结果留在持久账本；不启动假推理，不用最近输出猜“正在思考”，不伪造本机在线。此候选的 33 项受影响现有测试通过（含 native 子进程重开）；重新构建的真实 binary 两次无模型启动/退出均通过，genesis/head/identity 不变、`committed_step_no=0`、真实 home ready 与 Git memory 可读、C11 资产字节一致。证据为 `doescontainer/shenko-dev-live:/tmp/shenko-c05-final-_omvoejc/result.json` 与两份启动日志。首次观察窗仅 15 秒，而 Store 初始化已耗 14 秒，未算通过；修正为有限 120 秒观察后约 14 秒完成两次启动。最终合入状态见 PR 的交付评论。
- C11 已取代旧 WIP #58，具体源码/资产与验收边界见 [C11 卡](../tasks/C11.md)。真实 CLI 改名 r1→r2 后，已打开的 Chromium 页面约 580ms 内自动更新、导航次数仍 1、零浏览器错误；排序/退避/timer 收敛本轮仅代码核对。隔离 fixture 不是生产模型证据。
- 许可采用见 PR #64；`shwgdysjsvg` 已于 2026-10-25 12:20:53 UTC [明确同意贡献者协议](https://github.com/cyjin-yl/shenko/issues/54#issuecomment-6397563508)，主控已回应。协议版本和本人有权贡献范围见 [许可记录](LICENSING.md)；原作者/原 commits/来源 PR 保留。
- 全仓贡献者 monitor 在 doescontainer 的 `agent-proto-dev` 运行；脚本 `/tmp/shenko-gh-replies.py`，持久水位/事件为 `/workspace/shenko-gh-replies/`。覆盖普通评论、inline、reviews、新 PR/head/state；按端点完整分页成功后才推进水位，失败端点保留水位并在有限窗口重试。发现他人新事件即返回主控，或 20 分钟到期后续接；不能把脚本存在当成永久在线。
- 下一条主链是 C09 历史检索／重读 child、旧实例显式迁移及真实 backend KV/prefill；原生 Text/Image 与正式投影不重复建设。保留原始历史、角色和未完成调用，以 source window／leaf／profile/model generation/source commit seq 绑定安全边界；不重写朋友的历史/策略模块，不从文字上下文额度推测视觉能力或实际 KV 成本。其余包闭包、steer/stream、真实模型/QQ/多节点长期证据继续开放。

## 历史配额交接（以下为当时状态，已被上方当前接续覆盖）

> **2026-10-25 较早配额交接（当时模型额度耗尽，loop 停止）**
>
> 当前 provider 5 小时额度耗尽（HTTP 429），工作流停止；主控已完成本节的提交/推送/PR 保存，供 dot 或其他贡献者接续。不要声称本文件所列局部切片已完成完整 SPEC。
>
> **主线**：`df68c7cbf9ab0546aac59969c1e330fc7f306697`。
>
> **本次实际合入**
> - C20 窗口恢复修复：PR [#53](https://github.com/cyjin-yl/shenko/pull/53)（`9965b912`）——generation 与 append 同一 CAS、无需 operation 凭据时 Unknown、derivative 提交前 hash 校验、head DESC LIMIT1；PR [#50](https://github.com/cyjin-yl/shenko/pull/50) 随之显示 merged，朋友原提交/作者保留。17 项相关测试通过。
> - C01 raw session 写入口：PR [#51](https://github.com/cyjin-yl/shenko/pull/51)（merge `a2510c2497c6c2ebc65159b566b7a84ee0530b45`）——父行与事件同事务、幂等重放、冲突整批回滚；候选+当时最新 main 组合 16 tests 通过；closed-writer 语义保留。完整 #6 仍 OPEN。
>
> **开放 Draft PR（可直接接续，勿重复实现）**
> - C02 有限 native child：PR [#55](https://github.com/cyjin-yl/shenko/pull/55)，head `9eb26e1bb48c152af4922c682640bb6246fa7350`，base main。仅显式 `host.printf/v1` deterministic kind；两条 focused 场景通过；非通用 LLM child/生产 SSH/seat。完整 C02 未完成。
> - C04 Broker 公平并发与 wire 封口：PR [#57](https://github.com/cyjin-yl/shenko/pull/57)，head `5190d07`（含合并 main `a2510c2`）。 fairness/sealed-wire 修复；验证与真实后端 KV/四流证据仍需补齐。
> - C11 Proto-UI WIP：PR [#58](https://github.com/cyjin-yl/shenko/pull/58)，head `832b0e8e1e0f8c015ff6c5c06e42773ee9a4a522`，**未验证且不可合并**：基于旧 main `5ddc0ee`，在该基线不编译（`OutputAction::Delegate.definition` 漂移先于本 diff）。接续须从最新 main 重新应用 5 个 shenko-ui 文件、完成真实 Proto-UI `2962d4ee877f5a3544a13d4deaa09b6b2b038fb7`（Node24/pnpm10.32.1）构建与浏览器验收。
> - #52 朋友原地 update 校验：仍 OPEN/Draft，head `3271121`，定向验证因额度未完成，未合并。
>
> **未完成的治理决定（owner 已明确方向，文本未落地）**
> - 许可：owner 本人 `cyjin-yl` 作为再许可主体；社区 AGPL-3.0-or-later + owner 保留闭源/商业双许可；DCO 仅证明来源，不自动等于商业再许可。公开讨论 [#54](https://github.com/cyjin-yl/shenko/issues/54) 朋友尚未回复；当前未改 Cargo.toml（仍 Apache-2.0 元数据、无 LICENSE 正文），cordis-rs 为 MIT。
> - 协作规则：每个 agent 必须 monitor 自己创建/认领的 issue/PR；需回应评论 3 小时内回复或明确“正在处理+进度+下一 checkpoint”；超时无回应允许主控接管/转派，保留原作者/commit、以原 PR 来源+正常 merge 交接。尚未写入 AGENTS/GOAL/PROMPT。
>
> **可复用研究（容器 `shenko-dev-live`）**
> - OMP/Codex compact 对照已完成：OMP main=`0b5c27ab24719c86a5986b95ee55f4e75bc1aaa0`（包 18.12.6；tag `45c35e40`），Codex=`a3d93e1e692f1144cf8685175a5d1f6bf7294f57`；OMP 已有 Silver TTF CJK/52 shadcn family/compiler private；NPM `next` 0.3.0-alpha.1 仅 12 family，不可冒充 main。源码缓存 `/workspace/omp-compact-source-20261025-K12GSjIy`（`metadata/source-manifest.json` 含所有 SHA/permalink）。
> - Proto-UI：main=`2962d4ee877f5a3544a13d4deaa09b6a2b038fb7`；Rhea 来源 shadcn-ui `25af02b1`，New York `02cad611`；源码 `/workspace/c11-proto-ui-upstream-2962d4ee`。
> - 视觉端点：Tailnet vLLM `qwen3.8-27b`（OpenAI）/`qwen3.8-27b-anth`（Anthropic）已用真实 snap 帧验证可读图中英/数字/路径；证据见 #25 comment6389530566；非完整视觉精度/KV 验收。
> - 本地快照（未推送，仅供追溯）：`/tmp/shenko-c22-control-s1-20261025`（13 文件 554 行，已被 corrected lineage 成熟吸收）、`/tmp/shenko-c22-dispatch-candidate-3eqan12g`（基于已纠正的 wrong-author `3e6ad79`，勿推）、`/tmp/shenko-frozen-input-qxrlc_bp`（patch SHA `6136d8c0…`）。
>
> **下一动作优先级**：① 预算法恢复后继续 #52 定向验证与合入；② C04 补真实公平/KV/unknown 证据；③ C02 接真实 child handler→C07/T2；④ C11 从最新 main 重建 Proto-UI seam；⑤ 落地许可与 monitor/3h 规则文档；⑥ C20 执行 ledger join/版本 payload/正式 loop；⑦ #6/#12/#22 与 S1–S5 其余正式义务。


## 当前 C01 raw session 写入口与 C20 接续（非完整包完成）

- **实际 main 与原作者**：[PR51](https://github.com/cyjin-yl/shenko/pull/51) 已 normal merge，原 head `5255e5bc18fae86d6d9fa7caf227281599ba6c4f`，merge／本次产品基线 `a2510c2497c6c2ebc65159b566b7a84ee0530b45`，tree `93745c763e443ee68a4f029c6a70a28477f6c718`，另一 parent 是当时 main `9965b9125a11c2b87185aca0dbca644bebd1f33f`。原 commit 的 author/committer 均为 `shwgdysjsvg <2738635606@qq.com>`，且已是 main 祖先；未 squash、强推或改写朋友分支。
- **focused 交付边界**：main 现有 `RawSession` 与 `WriteBatch.raw_session_upserts`；ledger `try_commit` 在同一个 Immediate 事务内先写 raw session 父行、再写 `session_event_upserts`。同事实 replay 为 no-op；既有行的 agent/started_at/data 不一致，或同一 `(session,seq)` 对应不同 event_id，会报错并回滚整个 batch。原事件写入会静默忽略后一类冲突的问题已在此切片修复，不另设旁路 writer 或关闭外键。
- **已有验证，不由本文升级**：精确候选与独立 normal-merge 组合 `9965b912 + 5255e5bc` 的 tree 均为上述 `93745c76`；两轮指定 `cargo test --locked -p shenko-store -p shenko-types -j1` 均实际 exit 0，各 16 tests passed。首次 ezra 构建因 root-owned target 在编译前 exit 101，不计为通过；有效结果由 root 构建席位取得。命令环境与日志见 [PR51 合并说明](https://github.com/cyjin-yl/shenko/pull/51#issuecomment-6392010544)，路径为 `/tmp/shenko-pr51-review-5255e5bc/`。本次只更新文档，没有重跑 cargo、全 workspace 或新增测试，不声称 `verified_real`、完整 C01/C20 或 CI 通过。
- **closed-writer 语义保留**：同 seq/agent 等冲突继续沿既有 `with_mutation(|_| true)` 路径关闭写入方，测试重开 store 后核对回滚；这是当前合同下的既有行为，不误报为本 PR 新 bug。若需区分逻辑冲突与 `storage_failure`，可另做独立小切片；不要求 PR51 增加兼容模式或扩大范围。
- **C20 真实接续**：[PR50](https://github.com/cyjin-yl/shenko/pull/50) 原 head `3a1e63aeda0d4091dfbb1ed5f7ca4c421208ff92` 已是 main 祖先，[PR53](https://github.com/cyjin-yl/shenko/pull/53) 的修复 head `74732c6b6b82a9e884fe8ff30bc306e043c3c7a1` 已 normal merge 为 `9965b9125a11c2b87185aca0dbca644bebd1f33f`。PR51 已解除 raw session 父行写入阻塞；HistorySource／窗口 fixture 现在可以读取正式 `WriteBatch` 产生的原始会话，不再以独立连接模拟未来 writer，见 [#25 交付说明](https://github.com/cyjin-yl/shenko/issues/25#issuecomment-6392011548)。版本化 payload 读取、完整 OperationRecord/attempt/T2 关联与 `ended_at`、正式 loop、真实 KV/prefill／模型读图和包闭包仍是各自后继义务。本 checkpoint 读回时 [PR52](https://github.com/cyjin-yl/shenko/pull/52) 为 OPEN/DRAFT、head `3271121a2c2f68db6e94026ea40d20373a130fb4`；本文不对它新增 review 或合并声明。
- **状态与下一最小工作**：#6、#25 均保持 OPEN。C01 继续真实 child/T2、outbox/selector 与实际恢复交接；C20 从当前 main 接续版本化 payload 和执行 ledger 关联，再接正式 loop 安全边界。局部写接口、持久适配及本 docs checkpoint 均不关闭完整工作包或 S1–S5。

## 既有 C01/C22 F2 基线（较早 checkpoint；已独立复核并整合，非完整包完成）

- **真实栈/作者**：[PR41](https://github.com/cyjin-yl/shenko/pull/41) 已从 ready 正常合入 corrected 父38，merge `e7e26a6bd799584b98982cb859da3328cf0634e7`，完整 tree 等于被审查 head `2529968b56c92dcd044db3596628f6a5b0d04d60`（tree `031d1ff2af036e44d3cab79bfbcc97335f939785`）。七个朋友原 commits/作者与父图均保留；wrong-author `3e6ad79` 没有进入新图，旧来源 PR29/34 分支未改写/删除。
- **独立审查实际完成**：只读 `/tmp/shenko-c01-intent-review-sND5Yz69` 实际读回上述 SHA/tree，检查完整 `29fa07b..2529968b` composition 与 `feab9b6..2529968b` focused implementation，结论 `approved_slice`、findings=[]。不是只启动 reviewer 或创建 clone；不代表完整 C01/C22、生产 child/QQ 或 S1–S5 通过。
- **整合候选**：normal merge 最新 main `011cce9b81cb3a14306c9bd9aae451742f2f6bf2` 后产品 SHA `2169615684c30c5e2d8ef76e12090303c781d595`；与审查 child 的差异仅已合 C17 node ownership/evidence 修复及 docs。实际读过该 composition，单次 combined build exit0，复用原 F2 进程旅程在此 SHA 原样重跑 exit0。tracked Rust/Cargo/helper source SHA256=`21b9b617aa46440d0712dd9e28d5e273c2d16b74cbcf50ab2ff482599e766bba`，算法及输入路径见[本次证据](../evidence/c01-committed-intent-handoff-20261025.json)。随后只改证据/HANDOFF/任务状态，不把新文档 SHA 冒称重跑产品。
- **行为/每项恢复**：S1 同事务保存完整不可变 intent、ready/progress；memory 用原 helper 原子提交 entry+settled，一条 entry；delegate 在同笔 S1 交接为稳定 ID 的原生 Pending TaskRecord，不冒充已执行 child；publication 复用 Task/T1/Operation/Attempt/E1/OutboundLedger。两次真实 SIGKILL（S1 后与 armed-send 后），runtime 真实重开后先保留 Unknown；旧 owner 不能再次发，新 owner 由迟到 E1 结算。最终 3 settled handoffs、2 稳定 tasks、1 attempt、**1 wire send**；重复恢复不增发，稳定 UI WS 读回原 step/message/commit。已移除易失 postcommit 执行路径，不保留双执行 fallback。
- **真实/fixture 边界**：loopback WS 与进程 kill/reopen、SQLite/对象持久读回均真实；S1 由既有 probe 调真实 Store，恢复结算由同一 probe，不伪称完整生产模型旅程。迟到回执是明确的 out-of-band loopback provider-fixture query/injection，非生产 QQ 查询；恢复结算零模型调用/响应，另有一个被阻塞的 successor HTTP 请求。原 C22 R0/length/binding/shutdown 与2个 bin tests 为精确 `2529968b` 的有效证据；27 tests 只属于日志记录的较早输入。本次零新测试函数/独立新场景，无 CI 绿灯或物理双机/掉电声明。
- **旧 main ledger P1 不再误套本候选**：#6 comment6388656298 对当时 main 的新 ledger_id/seq 重置诊断真实；本栈继承的持久 `ledger_identity`、IMMEDIATE 事务分配序号在正常整合时保留。两套实际 F2 重开数据均为18 commits、1 ledger ID、seq1..18（18 unique）、5 writer incarnations。旧 mixed-ledger 历史原 refs 不改写，不伪造成从来只有一条历史；迁移/历史对账与更广 writer-CAS 合同仍 partial。
- **实际交付/下一动作**：[PR38](https://github.com/cyjin-yl/shenko/pull/38) 已先从 metadata successor 显式转换为 broader 49-file C01/C22 integration PR（真实 base main011cce9，不称 focused），ready 后正常合入 main：head `5c8cb8c3a00d37ab8e0fcb3b5c2501ae38c4cd0b`、merge/独立读回 `9d6c1af980ded1c4e8c7cd4dc24acd7405b92330`、tree `1aa5e100f7b40e93054992669a61af7720e636a9`。GitHub PR38/41 均实际 MERGED；PR28 随保留父图自动显示 merged（原图 `400c66e`），不是先把有 P1 的旧父独立交 main。全部7个朋友原提交现在确为 main 祖先。GitHub checks=0，不冒称 CI。最终交付见 [#6 comment6388977029](https://github.com/cyjin-yl/shenko/issues/6#issuecomment-6388977029)／[#5 comment6388977315](https://github.com/cyjin-yl/shenko/issues/5#issuecomment-6388977315)。本次后续纯文档 checkpoint 不改变上述产品输入。#6/#5 保持 OPEN。下一独立行为工作是原生 Pending task→真实 child admission/handler→C07/T2、有效 authority/祖先取消/node fencing 与 residual reconciliation supervisor；不是再建一套 task/outbox。
- **接续资源**：统一 locator `host=doescontainer/container=shenko-dev-live`；自有 repo `/tmp/shenko-c01-integrate-ukwtuqr6/repo`；原证据 `/tmp/shenko-c01-f2-83kqs2y8`；本组合 fresh-state probe/driver/binary/logs/hashes `/tmp/shenko-c01-integration-f2-jyq00wi7`，构建/source manifests `/tmp/shenko-c01-integrate-ukwtuqr6`。原候选、review snapshot、朋友目录、旧数据未共写/删除。Cargo 共享锁、jobs1、incremental0；本阶段未再开 worker。

以下 C17/C07/PR28 段落保留为各自 SHA 的历史 checkpoint；其中“F2/ledger 未修复”不覆盖上方已验证新组合。Unknown 历史 effect、真实 provider 查询/取消协调、完整 child/seat、生产 QQ/模型、durable compute/submission/完整 chunk stream、SQLite WAL-reset/CAS 掉电与 S1–S5/7–30天全部正式余项仍开放。

## C17 PR40 focused repair（2026-10-25；非完整包完成）

- 本轮 assign/comment 身份 cyjin-yl；Git base main `04c09a4051ba8641ef50265ee572aa373ab453bc`，
  branch `fix/c17-store-ownership-evidence-20261025`，
  [PR40](https://github.com/cyjin-yl/shenko/pull/40)，产品 `453fff7e754e46af920dc7fcbc46c9321f41d819`。
  **已实际ready并normal merge**：交付head `b17ec86f65ec7e082de74efec356c68e3a28f643`，merge/独立读回main
  `d593acfff33f1fa0f881be2497ee47f6784f396d`。该main完整tree等于已测候选；不是仅有本地代码。
  [实际交付comment](https://github.com/cyjin-yl/shenko/issues/22#issuecomment-6388655564)。GitHub CI checks=0，不冒称CI绿灯。
- 修复PR37引入的两项P2：未知数据库先immutable只读识别，不再因文件名/user_version=0改写；
  只迁移完整匹配的已初始化v1，保留十张旧表内容和身份/账本/seq；认证已知schema的完整
  receipt/source先独立E1，再判断task/T2，unassociated/invalid可在无模型重开后有限导出。
- 两条继承旅程、十五项既有相关Rust tests的适用版本、实际source/binary/schema/hash、
  receipt/原日志/失败边界见[修复证据](../evidence/c17-ownership-evidence-repair-20261025.json)。
  场景仍为同一受管容器内独立逻辑节点+真实OpenSSH，不是物理双机/LLM/选主/掉电验收。
  最终build与两条旅程exit0；15项原相关tests在7cf04bd实过，最终仅schema过滤字面修正，
  那15项包输入实核未变，未无谓重编/假称重跑。docs/fmt/diff exit0。错误计算predicate分支
  只做静态E1边界确认，不伪称另注入一次故障。自有sshd/proxy均停，连接errno111。
- 本轮未改C01/C22代码。#6 ledger_id/seq重开P1仍存在；主控已在#6认领S1后durable
  dispatcher，勿在其相同候选重复写。下一行为包为C01持续账本/dispatch→#12真实child/T2
  →#22 registry/成员authority。未知schemaIngress、自动pending协调、全部异构平台、
  A42/A47/A48/A49和7/30天原义务保留；#6/#12/#22及完整goal均不关闭。
- 独立候选 `/tmp/shenko-c17-repair-LzjgwLDo/repo`；原共享源/原worktrees/旧fixtures未写入或删除，
  Cargo使用共享flock、单job、关闭incremental；只使用本次loopback fixture，无陌生设备部署。

### 本次独立读回的关联PR状态

| issue / slice | 实际PR状态 | 交付/merge事实 |
| --- | --- | --- |
| #6 C01 | PR28 OPEN/DRAFT | head `ee9a3e009bec6d32437fec5cb9fbc26557d23204`；没有真实merge SHA，恢复/outbox及ledger P1不因C17完成 |
| #12 C07 | PR33、35 MERGED | parent merge `54d6ae5c0a870eee8640d2deb0e89854ae6dc1ae`；repair merge `94b8c5c2b03daaa7f3f73892e963479021ea6e24`；production child/T2仍欠 |
| #22 C17 | PR37、40 MERGED | first slice merge `012a3f6285210888ce1c5bbb0e23397ab9cfbfd3`；本修复merge `d593acfff33f1fa0f881be2497ee47f6784f396d`；#22仍OPEN |

这里是本次checkpoint时的真实远端观察，不将后续docs commit当未验证的新产品；完整S1–S5未完成。

## C07 当前可取修复与接续（2026-10-25）

- **最终远端交付已独立读回**：子 PR35 实际 MERGED，merge=`94b8c5c2b03daaa7f3f73892e963479021ea6e24`；父 PR33 实际 MERGED main，merge/可取产品基线=`54d6ae5c0a870eee8640d2deb0e89854ae6dc1ae`。该 main 的全部 tracked Rust/Cargo/helper 输入 SHA256=`f5a91f75ff5ad722f356e54e7d9b5a378e6e41ca569e8f861d5a16faff64c929`，与已验证组合候选一致；没有把 PR 接口缓存的 base SHA 当当前 main。交付评论见 [#12 checkpoint](https://github.com/cyjin-yl/shenko/issues/12#issuecomment-6388000869)。
- **下一动作与包边界**：#12 保持 OPEN/assign `cyjin-yl`；在真实 C01 outbox/task 接口上推进 production child/T2、authority/ancestor/node fencing 和 residual supervisor，并独立完成 seat/TUI/OMP/Claude/public-key UI/WASM。PR28 仍 OPEN/DRAFT，不是本轮完成前置恢复的证据；11 个完整 C07 case 与 #22 第二节点验收不据此通过。后续纯文档提交不改变上述产品输入；共享源未同步或回灌。

- [#12](https://github.com/cyjin-yl/shenko/issues/12) assign `cyjin-yl`；自有 `stack/c07-remote-exec` / [PR35](https://github.com/cyjin-yl/shenko/pull/35)，真实 Git 父 [PR33](https://github.com/cyjin-yl/shenko/pull/33) @ `727b96473835c4d2928a704ed7023d6995359740`。产品修复已 push `3846e9d8421e31fa01adeaf077e9864487dc5ff3`；这里不覆盖下方 C01 未解决的 P1，也不把语义前置当已验收。
- 五项 runner/store 阻塞已有实际修复与证据：排队写入的失败封闭、共用 seq/commit 排序、CAS 迁移后 host trust、upload 原子 0600、不可被重复 receipt 清除的持久 outcome 冲突。原 13 tests、build/check、真实隔离 SSH 旅程、SQLite 故障/并发与重开组合均成功；无新仓库测试。旧 helper receipt 的独立备份可查询，不盲重发。
- 已组合真实 main `98af8144788c5197dfe9289800127066d9ab5700`，本地 verification SHA `1dfa8252e56d0115f55316f53c3e04fde7e9fa5e` 的 application check/example build exit 0。原始输入/日志/范围见 [C07 修复证据](../evidence/c07-ssh-repair-20261025.json)、[任务卡](../tasks/C07.md)；PR 的实际 mergeCommit 和 issue 交付 comment 记录最终合入状态，不能以候选 SHA 冒充 main 已同步。
- 接续副本 `/tmp/shenko-c07-repair-owzisjCq/repo`；Cargo 仍共享 flock、单 job、关闭 incremental。自有 sshd 已下线、临时 keys/receipts/日志保留；共享源与原 worktrees 未写入/删除。下一个行为切片是 production durable outbox/child scheduler→SSH/T2、归属/authority/祖先取消/node fencing 与 residual supervisor；完整 seat/OMP/Claude/TUI/WASM/第二物理节点和长跑未交付。baseline ledger identity/reopen 连续性、SQLite 3.46.0 WAL-reset/CAS 掉电保证仍有缺口。#12 与全产品均不标 complete。

## 历史 checkpoint：PR28 在 ee9a3e0 仍有 P1，未直接合入

- 执行身份 `cyjin-yl`；[#6](https://github.com/cyjin-yl/shenko/issues/6) 已 assign 同一真实账号，并有开工/接口/交付/本轮复核认领 comments。认领不形成排他锁或人工 gate。
- 复核对象是 [PR28](https://github.com/cyjin-yl/shenko/pull/28) / `fix/c01-durable-recovery-20261024` 的实际 head `ee9a3e009bec6d32437fec5cb9fbc26557d23204`，重点逐项读回 `b525f8d→ee9a3e0` 的13文件增量，而非复述 fixed_findings。
- GitHub PR 接口读回的 base SHA 为 `21b85e2cf272e0c2ce9e8e17de426696c59370cb`；本轮另外新 clone 的远端 main 为 `9a1a0a41d6d72add9fb3c5b7135f51ba7bc0c409`（PR30 已合入）。两者不是同一事实；本轮未复核 PR30，也未把 PR28 与其组合构建。
- PR28 实际状态仍为 **OPEN / DRAFT**、无 CI checks；本轮未 mark ready、未 merge，故没有 PR28 merge SHA。没有关闭 #6，也没有宣称 C01/全产品完成。

### 两个仍需修复的 P1

已在 [逐行 review](https://github.com/cyjin-yl/shenko/pull/28#pullrequestreview-5639070800) 提交真实调用链、触发状态和行号；这是当前 SHA 的静态确认，未伪称新增运行复现。

1. `bin/shenko/src/main.rs:169` 先启动恢复主体，NapCat 出站订阅到 `:284` 才建立。`ResultRecorded` 无需 broker 即可接续 S1；若 ingress 打开/迁移较慢，S1 后的 broadcast 先于订阅。保留但无人消费的 `_outbox_rx` 不会补转历史，新订阅也不接收旧消息。输入已持久处置、`publication_intents` 为空，回复没有对应 outbound operation，重放输入也不会补做。先建立实际消费者，再放行恢复执行，并保留已提交发布的稳定可补投身份。
2. `bin/shenko/src/main.rs:387` 在等待主体结算之前取消共享 token，同时结束 NapCat 接收/回复与委派消费者。在途模型随后真实返回时，主体仍能 S1/处置输入并正常退出，但回复/委派接收者已经停止；回复可能连持久 intent 都没有。分开 admission/主步停止与已提交工作消费者的生命周期，在消费者具备持久交接/恢复边界后再关闭。模型有真实结果不等于全部后续效果已结算，也不证明远端停止。

### 本轮实际证据与边界

- 全部实际命令通过 SSH 在受管 doescontainer 的容器执行；原共享源码、原 worktree 和未提交改动未写入。只在容器磁盘 `/tmp` 新建独立 Git clone；没有改权限、全局 safe.directory、分支保护或强推。
- 原候选 `/tmp/shenko-c01-recover-tcqxw1p2/repo` HEAD 读回为 `ee9a3e0`，工作区干净。按已公布算法（排序 tracked Rust/Cargo 路径 + NUL + bytes + NUL）重新计算 source SHA-256，确为 `46a7d2df69a3005b65d41e7aa09409967399895f78517fe6363bbbff3ee95e61`；`p1-fixed.bundle` SHA-256 确为 `46e2832133e01802a2dfe2fa597f654a6be079603bf985f8259dbcad4669f8ba`。
- 独立读取原始 `/tmp/shenko-c01-p1-tests-r4.log`：相关 suites 共23 passed、0 failed/ignored；复用此前串行 Cargo 证据，**本轮未重跑 Cargo、未新增测试**。原日志确有 >256槽背压接收、typed重试/冻结输入/结果恢复、未知请求阻断、CAS失败、竞争S1等通过输出。
- 独立读取 `/tmp/shenko-c01-process-l5dc3k9u/rejected.log`、`settled.log`：确有 HTTP429 失败退出、原实例重开、SIGINT 后等待真实模型响应再退出的轨迹。这只是 loopback 模拟；没有 NapCat/委派消费者结算的证据，不能覆盖上述两个组合窗口。source/hash与原始日志读回不等于本轮重新测得历史命令 exit code。
- socket/分页投递已真实拆开；安全 Preparing、带正面证书的 RetryPending 与真实 ResultRecorded 有恢复路径；generic/transport unknown 仍阻断；稳定源身份的昵称/内容冲突保留为非执行观察；主体返回错误/handle监督与UI易失 admission 的表达都有实际补丁。这些局部进展不抵消上述未解决的输出交接缺陷。
- 不补造真实 NapCat/provider 账号验收、32事件完整模型/ACK组合、第二节点/异构设备或7/30天长跑证据。selector/多分支/full writer fencing、完整 genesis/launch、S1后 durable outbox/task/memory replay 和持久 Web ingress 等仍是后续义务。

### 下一动作

从 PR28 已推的实际 head 建 focused continuation 分支修复两个 P1，保留原作者/提交和朋友后继栈（含 PR29），在 #6/PR comments 说明接口、base/head及checkpoint；不用等待认领释放或额外人类审批。复用已有恢复/停机旅程，不建立新测试工厂。验证通过后再按真实权限推进 ready/合入；main 已有 PR30，届时须核对并验证真实整合候选，不能把各分支结果直接相加。#12 SSH 和 #22 第二节点各自独立认领/PR，可先推进不依赖缺失前置证据的部分，不冒称 C01 全链已通。

本次仅更新交接文档，不改产品代码/覆盖计数；记录本文的 docs 分支为 `docs/c01-independent-review-20261025`，提交/PR/合入结果由其真实 GitHub 记录及 #6 接续 comment 定位。

## 历史基线（保留来源，不代表当前 main 或本轮复测）

以下内容源于旧 checkpoint `093b417`、PR3及2026-10-24协作记录；26个测试、coverage状态、资源与推荐顺序均不是本轮新测结论。与上方当前 checkpoint 冲突时以上方为准。PR30 的真实 cordis 接线已在 main，不再沿用下方“尚无 runtime 绑定”作为当前事实；本轮没有独立评审它。

## 工作区事实（已核对）

- 18 个 workspace package：`crates/` 下 17 个库 crate + `bin/shenko`（binary）。
  库：types、kernel、store、broker、subject、memory、channel-napcat、
  channel-napcat-link、exec-host、exec-ssh、wasm、compact-budget、compact-snap、
  awareness-core、ui、seed、update。
- Rust **2024 edition**（workspace `rust-version = "1.85"`），wasmtime 45。
- 测试：`cargo test --workspace` = **26 passed, 0 failed, 0 ignored**，分布：
  store 11（内联 3 + memory_search 3 + store_roundtrip 5）、wasm 3、
  awareness-core 3、types 2、channel-napcat 2、memory 2、update 2、ui 1。
  其余 9 个 crate 尚无测试。
- `docs/state/coverage.json`：147 项 R 中 **27 partial / 120 not_assessed / 0 satisfied**；
  `coverage_status = s1_s2_partial_verified`。

## 已具备的能力（真实状态）

- Store §41.2：SQLite WAL + CAS + ledger + FTS5（CJK 走 LIKE 回退）。
- Subject §5.1/§41.5：连续 S0/S1 loop、S1 决策记录、genesis CAS 封存、outbox broadcast。
- Inference §13/§47：Anthropic broker 优先级队列；本地 Qwen3.8-27B OpenAI-compat provider。
- Channels §16：NapCat OneBot WS 接入、IngressSpool dedup/gap、OutboundLedger
  intent→ack→unknown→reconcile（15s 超时=Unknown，不盲重发）。
- WASM §8/§29：wasmtime 45 Engine/Module/call_func，WASI p1 command 执行，
  `/home` 持久 home preopen；checkpoint 目前只写 `.wasm_checkpoint` 标记。
- Self-dev §35.3–§35.6：BuildProvider（worktree + cargo）、TmuxSeat/SeatObservation、
  HookRegistry 4 阶段、EffectContract 5 种动作、SelfKnowledgeProvider。
- Self-upgrade §35.7：`shenko-update` A/B 槽位 + rusqlite 一致性备份 +
  Prepare→CatchUp→Smoke→Handoff→Observe→Retire 状态机（smoke 为本地离线检查）。
- Playground §43：`/playground` CRUD、原子写、`artifact_changed` inbox 事件。
- Awareness §45/§38：独立 awareness 单元安装/启用/停用 + SelfKnowledge。
- Plugin §7：Plugin trait 按 cordis-rs rev 1754062 的
  Plugin/Context/Service/effect-cleanup 形状设计；`cordis-core` 已在 Cargo.toml
  声明但尚未在 runtime 绑定，也无 crate 使用 cordis 类型。

## 仍存在的主要差距（按审计真实现状）

| # | SPEC 条款 | 状态 | 说明 |
|---|---|---|---|
| 1 | §7 cordis 插件主干 | 未接 runtime | trait 为 Shenko 自有 facade；`cordis-core` 已声明但零引用；TODO 在真实 cordis 生命周期上托管插件 |
| 2 | §8/§29 WASM 家园 | 部分实现 | engine + WASI p1 + home preopen 可用；`MountSpec` VFS 未接线，真实家园/首启未接，checkpoint 仅标记文件；`WasmRuntime` 在 crate 外无调用者 |
| 3 | §35.7 自升级 A/B | 部分实现 | 状态机与一致性备份已具备；`shenko-update` 不是 `bin/shenko` 依赖，quiesce/catch-up/epoch 切换未接线 |
| 4 | §43 Playground | 部分实现 | CRUD + 原子写 + `artifact_changed` 已具备；`POST /playground/{id}/backend` 仅回显，真实进程后端未做 |
| 5 | §12 记忆循环 | 部分实现 | FTS5/LIKE 搜索与 intents 存储已具备；thought loop 已于 S1 后消费标记内容并提交 intent（093b417），`spawn_worker` 仍无调用者 |
| 6 | §35.3 Compact | 部分实现 | Budget/Snap 为独立 crate，未注册为插件、未进入默认插件闭包 |
| 7 | §35.4/§35.5 seat | 部分实现 | `TmuxSeat`/`SeatObservation` 位于 `shenko-exec-ssh`；`main.rs` 仍是 TODO，未绑定 |
| 8 | §5.1/§15.2 QQ 出站 | 部分实现 | UI broadcast + QQ ledger 已具备；真实 QQ 端到端回执未验证 |
| 9 | §44.6 seed/bootstrap | 部分实现 | genesis 经 `cas.seal_object` 封存；seed 内容装配、WASM 家园、首启 probe 未完成 |
| 10 | §16 NapCatLink backend | 可选实现 | `shenko-channel-napcat-link` 的 `napcat-link` feature 默认关闭，非 bin 依赖，未接入 |

## 下一步推荐工作（按因果链）

1. **§7**：把 `cordis-core` 真正绑定到 Plugin 生命周期；删除或落实 facade 的 TODO。
2. **§8/§29**：`MountSpec` VFS 接线 + 真实 checkpoint 快照 + 首启 WASM 家园。
3. **§35.4/§35.7**：TmuxSeat 接入 main；`shenko-update` 接入 runtime 完成 A/B handoff。
4. **§12/§35.3**：memory intents 由 thought loop 消费；Compact 注册为插件。
5. **§44.6**：完整 seed 装配与首启 probe。
6. **验收**：V1-01~V1-15 尚未执行。

## 资源

- Qwen3.8-27B 端点：https://doesvm.icefish-exponential.ts.net:8446（Tailnet，无需真实 key）
- SPEC §22.1 基线模型：`swift-qwen3.8-27b-nvfp4`，262144 token 上下文
- PR #3：https://github.com/cyjin-yl/shenko/pull/3（已合并）

## 验收计数

- R：27/147 partial，0 satisfied（以 `docs/state/coverage.json` 为准）
- 测试：26 passed / 0 failed（见上）
- 待验收：V1-01~V1-15 均未执行

## 社区任务栈接续（2026-10-24）

- [#4总追踪](https://github.com/cyjin-yl/shenko/issues/4)、22个工作包#5–#26已发布；[任务图](../plans/active/community-stack.md)、[朋友PROMPT](../../PROMPT.md)。归属映射不改变实现/验收状态。
- 当前公开代码盘点基线`d4b612b`；此文档增量不含产品实现和新构建结果。
- 恢复线暂停，本地`291d5a5`候选及ACK接线保留，32入站只渲染10却全ACK的问题仍待验证。Broker/SSH、UI和cordis候选排队，不当作已merge。先#6，再#12，再#22第二节点接走任务。
- 朋友离线不锁任务：从最后已推checkpoint开continuation分支，保留作者/提交/PR，不等交还/审批，未知effects先对账。
