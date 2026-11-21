# 验证证据

此目录保留各 focused 切片的源码/工件绑定证据；它们不是完整 Shenko 或 S1–S5 通过声明。每条记录区分候选、实际输入、环境、方法、结果、失败与尚未覆盖的 R/B/case。初始化文档完整性报告只证明交接材料一致。

- [DSH 工具桥接与原始计算器插件验证](dsh-bridge-2026-11-20.md)：保留原计算器旧依赖与新宿主经真实 MCP stdio 返回 96，以及各次宿主协议检查的历史结果；后续 [Store/native 接续验证](dsh-native-2026-11-21.md)绑定首次实测候选的 8 个 JavaScript 用例和 1 条原生旅程，归档 Unknown 不重放、同进程 Store 关闭重开与撤回的实际回执。模型选用、OS 崩溃恢复与唱歌仍未验证。

- [Store 留存审计、准备身份与原件导出的后续候选](retention-preparation-export-20261116/README.md)：保留逐字节整合复核、原作者源码、三组各不超过九个窗口的边界及零新增行为的导出接线；本地未执行 Rust。工具 manifest 的类型／同摘要别名漏口已修；全槽位 GC、全安装写入排他和完整自开发仍待后续。Clippy 归档误分类的原失败与输入闭包缺口另行保留，未放宽门禁。
- [CI 3e62：Rust 通过、Clippy 失败与成功原件缺口](ci-3e62c0e5-20261116/README.md)：实际 checkout `1c01c95a`／tree `1ccf8f0b`；1,094 通过、0 失败，原 E1 后继旅程通过。四完整 ZIP／1,904 成员、52 份 native E1 原件及五份完整 job 日志保留；Unknown、缺失对象债务和两次实际副作用均可追溯。S2 build／PTY 测试通过，但部分成功原回执被 fixture 删除或捕获，明确保留不能回填的缺口；Clippy 七条归档误分类及真实绑定告警未冒称解决。
- [CI 10d4：编译通过，E1 后继查询与 Clippy 失败](ci-10d4a325-20261116/README.md)：实际 checkout、四份完整原 ZIP／1,900 成员、五份完整 job 日志与所有 native E1 原件保留；1,088 通过、1 失败、11 忽略。后继查询读取不存在的列，单谓词修复仍待新 CI；Clippy 的原归档误分类和 fixture 警告分别保留，未删改原件或放宽门禁。
- [S2 本次实际构建产物与持久回执候选](s2-build-receipt-20261116/README.md)：使用 Cargo 原 metadata/JSON 产物及每次独占输出，保存完整有界原流与 typed CAS/raw history 根；五个新窗口和三个原拒绝窗口待实际 Rust 执行，不冒称测试后自修改或完整 S2。
- [PTY 真实本地 write/flush 回执与源码复核](pty-write-review-20261116/README.md)：同 FIFO 的本地传输观察、原流取消和 Unknown/NotSent 保真；五个协议窗口未运行，两个 fixture 顺序/清理 finding 已修，完整原静态结果及作者来源保留。
- [Node-WASIX 固定 header 相对链接候选](node-wasix-relative-header-20261116/README.md)：仅修固定上游一个指向作者机器的绝对 symlink，真实同树 header 原字节不变；三个有限 Git patch/来源拒绝检查实际通过，全部命令和输入绑定保留。工具链、预算、原 guest 及完成判据不变，没有第三次源码构建或正式能力通过。
- [第二次 Node-WASIX 源构建：绝对 header 链接失败](node-wasix-source-hosted-43945415290/README.md)：完整原 ZIP 的 121 个成员、实际 checkout、工具/源码/资源和全部原输入保留并独立核验。构建越过 OpenSSL 归档命令后，因固定上游中的绝对 symlink 无法打开 V8 header 而失败；原 8 个符号链接已只读核对。没有 module、cell、reopen 或正式能力通过，前轮失败仍保留，未重跑或调高限额。
- [C19 directory-lock integration and actual local checks](c19-directory-lock-integration-20261116/README.md): adopts PR #314 on this integration branch, preserves both source revisions/authors and the alternative PR #313 review. The integrated 15 existing + 2 concurrency scenarios passed locally; the existing subprocesses are now bounded. Read-only report support and local serialization do not prove a 7/30-day run or physical durability.
- [CI a1d3：E1 fixture 编译失败与精确修复](ci-a1d3fa6e-20261116/README.md)：保留实际 merge checkout、独立 baseline、四个完整原 ZIP 和五份完整解码 job 日志；四门通过，Rust/Clippy 因同一 moved-value 错误失败，另有真实 production `large_enum_variant` 增量。Guest 与 workspace tests 全部跳过；两个零新增场景的最小源码修复仍待下一候选 CI，原失败不改写。
- [83bf CI 的六处 capture 失败与原件](ci-83bf3914-20261116/README.md)：实际 checkout `2d275430`／tree `d695242e`、独立 baseline `84ae1940`；五门成功、Rust 六失败，四份完整 ZIP 和可取得的完整日志均保留。E1 导出明确部分失败；非 pinned SQLite 诊断与其两次初始失败也保留。随后源码修复仍需新 CI。
- [GC 根声明的一致性边界](../development/cas-root-declarations.md)：校验实际 producer／namespace／root report 的一致性，原旅程增加五拒绝与一恢复窗口；源码已审，尚未运行，不等同 Store GC 或共享 CAS 排他完成。

- [CI 51a1：三进程通过、首次 E1 准备失败](ci-51a1ce43-20261116/README.md)：保存完整 Rust job 原文、API 源码与实际 merge checkout/tree 绑定、取消记录和两次 Clippy 日志读取失败；三进程 SSH star 85.61 秒通过，五门通过。E1 因 schema 8 acknowledgement 在 SSH 前失败，九窗口未到达；713 只修 fixture，仍待运行，原 ZIP 尚待独立字节归档与核验。
- [CI bf49：新原生闭环与纠错容量旅程](ci-bf49f387-20261116/README.md)：Store 40/40 与 subject 新容量旅程实际通过；全 CI 仍因 metadata readiness 和精确 Clippy 增量失败。全部原始日志、manifest 和单独源码绑定的后续编译结果保留，未将修后检查改称该轮通过。
- [Node-WASIX 固定源码构建候选](node-wasix-source-build-20261116/README.md)：从具有 BigInt handler 的精确上游源码及固定官方工具链进行独立 opt-in 构建，再复用原三 cell／reopen；仅静态检查，尚无该源码构建或运行结果，不改变能力与完成声明。
- [首次 hosted Node-WASIX](node-wasix-hosted-43916483602/README.md)：保留真实 run 的完整 26 个 artifact 原件与源码/资源绑定；普通 exit 1，guest 明确报告 `BigIntConstructor` 未实现，未产生 cell/checkpoint/reopen。新 main 源码有对应处理，但没有已验证的匹配构建产物。
- [Node-WASIX 固定候选与 hosted 复现入口](named-node-wasix-20261116/README.md)：保留本机虚拟映射失败和主动 RSS 停止的全部小型原件；单一 opt-in 工作流复用固定 module、原 driver/输入及条件性 reopen。该入口提交时尚无 hosted 结果；Node/Bun availability、恢复等级与完整目标没有改变。
- [Canonical E1 续接与容量纠错整合](canonical-capacity-integration-20261116/README.md)：保留 dff/c3 的原始定向 Clippy、最终 fixture 元数据与文档日志；两次组合 Clippy 的默认 deny 失败也完整留存。bfc5 四包全目标元数据通过，98bc 脚本切片只有静态证据；新原生/纠错/真实模型旅程均未因此取得运行通过。
- [独立 native E1 传播：源码与元数据检查](native-e1-delivery-metadata-20261116/README.md)：绑定 `862213d9`，无损保留最初 CLI Option 类型错误、共享 helper 诊断及最终通过的完整 Cargo 输出；九个真实 SSH 旅程窗口仅已编写，尚未运行。两次早期文档失败的原始输出被覆盖，明确保留证据缺口；Store-aware GC 与排他尚未完成，namespace 漏报也不会被自动发现。
- [实际 PR303 基线与 main 整合](current-base-integration-20261116/README.md)：普通合并 e34efd1 与 bd30bee4，保留后者已合入的 main be7d09df、停机屏障和渠道诚实性改动；逐冲突保留现有 heartbeat、安装身份、Store/Broker 错误语义与全部原证据。CLI 元数据和文档守恒检查通过，新的进程旅程仍待对应候选运行。
- [CI 9b5a6cb：Store 与 SSH 通过、三进程前置失败](ci-9b5a6cb-20261116/README.md)：完整 job/workspace 原日志与全 ZIP 清单保留；canonical first claim、39 项 Store、精确零写/Handoff、SSH 和暂停重开实际通过。三进程旅程因跨节点比较本地学习时间而在成员核对处失败；后续 fixture 修复只取得 metadata 检查，不算运行通过。
- [第四次真实模型实验](single-node-hosted-43885905651/observation.json)：完整原 ZIP 保留 2,645 份原件；真实生成与 38 个主请求终态已取得，但控制协议纠错未产生 child，最终服务端明确拒绝超容量请求。容量字段读取与纠错重新计量的缺口尚待修复；不算单机闭环或 27B 行为通过。
- [CI 15cfee0 实际 SSH 与唯一 Store 失败](ci-15cfee0-20261116/README.md)：保留同候选完整 workspace 日志、SSH 安装/命名执行原件、120/180 秒时序和逐字节 SHM 差异；星形协议、native arm 与暂停重开旅程已实际通过，整轮 CI 仍因一个零写 fixture 断言失败。
- [主线 PR304 整合](main-304-integration-20261116/observation.json)：保留主线与候选两方来源、模块拆分后的完整元数据检查日志和冲突处理范围；原生执行、恢复、安装身份与全部原断言均保留。文档迁移和独立边界检查通过；此记录不包含新整合版本的运行测试。
- [Poppy 账号、签名传输与 UCP 连续性](poppy-operational-20261115.md)：Issue #297 / PR302，记录两个新增连续旅程、最终候选检查、公开取样及真实商家验收缺口；[首个增量](poppy-foundation-20261115.md) 保留发现与签名基础的历史证据。

- [统一 Node、安装同步与认知恢复](node-sync-cognition-20261026.json)：#71 / PR72，映射 C17/C10/C09/C01/C03；真实接收安装与模型不可用时的确定性恢复，同一 PVE 故障域，不宣称全 C17 或物理多机容灾。
- 较早的 [C17 首条独立节点旅程](c17-native-first-journey-20261025.json)、[ownership/E1 修复](c17-ownership-evidence-repair-20261025.json)、[SSH 安装回执修复](c17-script-receipts-20261025.json) 保留各自版本和更窄边界，不能替代后续主应用输入的验证。
- [C19 长跑观察工具](c19-longrun-observation-tooling-20261029.json)：#24；该记录本身的保守窗口准入、日志/重启/进程身份修复与 9 个独立场景清单；记录内的 schema 与场景数是它自己的历史版本，不能读成当前工具状态（当前工具为 schema 3 记录／schema 5 报告，要求实际打开执行文件的逐样本 SHA-256，见 [长跑观察](../development/long-run-observation.md)）。**不包含任何长跑结果**。当前工具回归与原生核验须使用各自源码绑定的新回执；旧自检手记不证明新合同或物理耐久性，不能当 S5 长期验收。
- [文档布局与源码追踪来源](documentation-layout-20261116.json)：记录导航原文与研究归档的搬迁位置、原始摘要，以及 coverage 中源码注释迁移的入口。`checks_run` 为空、`product_tested=false`；它是供整合验证消费的来源绑定，没有产品通过声明。

- [CI be46：启动取消通过、组件身份取证失败](ci-be46be34-20261116/README.md)：完整保留原失败、五份 job 日志、全部 Clippy 流及 artifact 成员摘要；悬挂 GET 的实际 EOF、暂停重开与真实 SSH 通过。组件误把 `/health` 当身份端点，后续改读同端口 `/api/self` 并严格核对主体与安装；完整进程后半仍待新运行。此前 pin 宏诊断漏判及独立修复检查也保留原件和更正。

大日志、录屏和敏感环境信息可以保存在合适的私有工件存储；仓库保留非秘密的内容摘要、hash、位置与复现方法。最终completion.json与报告按COMPLETION合同生成。不能提前生成一个success示例让goal误判产品完成。

- [7905 CI 六门通过与首次 E1 九窗口原件](ci-7905e841-20261116/README.md)：实际 419ca5c6 checkout；原 ZIP／完整日志与范围限制。
- [第五次 32K 真实模型失败与独立复核](real-single-node-model5-43916477826/README.md)：完整原 ZIP、40 次协议拒绝、最后 Unknown、能力说明缺口，未冒称闭环。
- [环境中断与固定源码恢复](environment-recovery-20261116/README.md)：原失败、只读诊断、原身份 shallow 恢复与 main 整合边界。
- [51a 原失败 ZIP 的恢复后补证](ci-51a1ce43-20261116/archive-recovered/README.md)：完整字节与成员校验追加，不覆盖当时记录。
- [262 CI 六门通过与完整原件](ci-262f980e-20261116/README.md)：实际 aa58209c checkout／1740cd95 tree、84ae1940 baseline；保留两完整 ZIP、完整 job 原文与首次 E1 原件。此结果不覆盖随后容量、留证、seat 或 Node 构建规则改动。
- [首次 E1 原始事实与 CAS 的安全留证](native-e1-retention-20261116/README.md)：成功和失败清理前保留选定原始行与有类型的对象闭包；两个 Python 导出窗口通过，新 Rust／SSH 运行仍待对应候选验证。
- [Canonical capture 容量与 once-evaluation 边界](../development/canonical-capture-budget.md)：同一事务中一次评估、流式完整性观察、预付行与 JSON 容量及既有 S1 准入退路。83bf 的容量旅程通过，但缓存／FTS 旅程在首处拒绝，后续窗口未到达；针对性源码修复保留原九窗口与预算，仍待新 CI，不声称消除了全表 I/O。
- [固定 Node 源码构建的完整失败原件](node-wasix-source-hosted-43922989826/README.md)：原 3,195.484 秒构建在 OpenSSL 归档处失败；完整 ZIP、stderr 与资源观测保留，没有 Node 链接、guest cell 或恢复通过。
- [Node 归档长参数修复与有限诊断](node-wasix-archive-recipe-20261116/README.md)：固定上游两处分支补丁；普通和 thin 响应文件规则保留 928 个输入及顺序，初次 ptrace 拒绝原件也保留。修复后的唯一完整构建 [43945415290](https://github.com/cyjin-yl/shenko/actions/runs/43945415290) 在此记录时运行中，尚无 WASIX 构建或 guest 通过结论。
- [原始作者与提交对象](source-checkpoints-20261116/README.md)：保存原始 Git commit／tree／作者与时间，不把来源保全误算成产品验证，待修的 seat checkpoint 也明确保留原状态。
- [托管 seat 生命周期候选与复核](managed-seat-lifecycle-20261116/README.md)：关闭取消保留失败、同 generation 的七个工具注册、六个新或扩展窗口；源码已复核，集成运行待验证。
- [首次 Unknown 的一个因果 E1 后继](../development/canonical-native-e1-successor.md)：保留原观察、acknowledgement 与来源，七个新 SSH 窗口不获得执行权限；没有用较早首次 E1 的通过代替后继运行结果。

- [Merged #339–#342 component observations](merged-components-342-20261120.json): original native receipt/input/executable bindings and protected CI heads; standalone/composed and schema-7/schema-9 results remain separate, including failures. No canonical acceptance case is newly satisfied.
