# 验证证据

此目录保留各 focused 切片的源码/工件绑定证据；它们不是完整 Shenko 或 S1–S5 通过声明。每条记录区分候选、实际输入、环境、方法、结果、失败与尚未覆盖的 R/B/case。初始化文档完整性报告只证明交接材料一致。

- [Canonical E1 续接与容量纠错整合](canonical-capacity-integration-20261116/README.md)：保留 dff/c3 的原始定向 Clippy、最终 fixture 元数据与文档日志；两次组合 Clippy 的默认 deny 失败也完整留存。bfc5 四包全目标元数据通过，98bc 脚本切片只有静态证据；新原生/纠错/真实模型旅程均未因此取得运行通过。
- [实际 PR303 基线与 main 整合](current-base-integration-20261116/README.md)：普通合并 e34efd1 与 bd30bee4，保留后者已合入的 main be7d09df、停机屏障和渠道诚实性改动；逐冲突保留现有 heartbeat、安装身份、Store/Broker 错误语义与全部原证据。CLI 元数据和文档守恒检查通过，新的进程旅程仍待对应候选运行。
- [CI 9b5a6cb：Store 与 SSH 通过、三进程前置失败](ci-9b5a6cb-20261116/README.md)：完整 job/workspace 原日志与全 ZIP 清单保留；canonical first claim、39 项 Store、精确零写/Handoff、SSH 和暂停重开实际通过。三进程旅程因跨节点比较本地学习时间而在成员核对处失败；后续 fixture 修复只取得 metadata 检查，不算运行通过。
- [第四次真实模型实验](single-node-hosted-43885905651/observation.json)：完整原 ZIP 保留 2,645 份原件；真实生成与 38 个主请求终态已取得，但控制协议纠错未产生 child，最终服务端明确拒绝超容量请求。容量字段读取与纠错重新计量的缺口尚待修复；不算单机闭环或 27B 行为通过。
- [CI 15cfee0 实际 SSH 与唯一 Store 失败](ci-15cfee0-20261116/README.md)：保留同候选完整 workspace 日志、SSH 安装/命名执行原件、120/180 秒时序和逐字节 SHM 差异；星形协议、native arm 与暂停重开旅程已实际通过，整轮 CI 仍因一个零写 fixture 断言失败。
- [主线 PR304 整合](main-304-integration-20261116/observation.json)：保留主线与候选两方来源、模块拆分后的完整元数据检查日志和冲突处理范围；原生执行、恢复、安装身份与全部原断言均保留。文档迁移和独立边界检查通过；此记录不包含新整合版本的运行测试。
- [统一 Node、安装同步与认知恢复](node-sync-cognition-20261026.json)：#71 / PR72，映射 C17/C10/C09/C01/C03；真实接收安装与模型不可用时的确定性恢复，同一 PVE 故障域，不宣称全 C17 或物理多机容灾。
- 较早的 [C17 首条独立节点旅程](c17-native-first-journey-20261025.json)、[ownership/E1 修复](c17-ownership-evidence-repair-20261025.json)、[SSH 安装回执修复](c17-script-receipts-20261025.json) 保留各自版本和更窄边界，不能替代后续主应用输入的验证。
- [C19 长跑观察工具](c19-longrun-observation-tooling-20261029.json)：#24；该记录本身的保守窗口准入、日志/重启/进程身份修复与 9 个独立场景清单；记录内的 schema 与场景数是它自己的历史版本，不能读成当前工具状态（当前工具为 schema 3 记录／schema 5 报告，要求实际打开执行文件的逐样本 SHA-256，见 [长跑观察](../development/long-run-observation.md)）。**不包含任何长跑结果**。当前工具回归与原生核验须使用各自源码绑定的新回执；旧自检手记不证明新合同或物理耐久性，不能当 S5 长期验收。
- [文档布局与源码追踪来源](documentation-layout-20261116.json)：记录导航原文与研究归档的搬迁位置、原始摘要，以及 coverage 中源码注释迁移的入口。`checks_run` 为空、`product_tested=false`；它是供整合验证消费的来源绑定，没有产品通过声明。

大日志、录屏和敏感环境信息可以保存在合适的私有工件存储；仓库保留非秘密的内容摘要、hash、位置与复现方法。最终completion.json与报告按COMPLETION合同生成。不能提前生成一个success示例让goal误判产品完成。
