# 验证证据

此目录保留各 focused 切片的源码/工件绑定证据；它们不是完整 Shenko 或 S1–S5 通过声明。每条记录区分候选、实际输入、环境、方法、结果、失败与尚未覆盖的 R/B/case。初始化文档完整性报告只证明交接材料一致。

- [统一 Node、安装同步与认知恢复](node-sync-cognition-20261026.json)：#71 / PR72，映射 C17/C10/C09/C01/C03；真实接收安装与模型不可用时的确定性恢复，同一 PVE 故障域，不宣称全 C17 或物理多机容灾。
- 较早的 [C17 首条独立节点旅程](c17-native-first-journey-20261025.json)、[ownership/E1 修复](c17-ownership-evidence-repair-20261025.json)、[SSH 安装回执修复](c17-script-receipts-20261025.json) 保留各自版本和更窄边界，不能替代后续主应用输入的验证。
- [C19 长跑观察工具](c19-longrun-observation-tooling-20261029.json)：#24；schema 2 的保守窗口准入、日志/重启/进程身份修复与 9 个独立场景清单，**尚待父级执行验证，不包含任何长跑结果**。旧自检手记不证明修复或物理耐久性，不能当 S5 长期验收。

大日志、录屏和敏感环境信息可以保存在合适的私有工件存储；仓库保留非秘密的内容摘要、hash、位置与复现方法。最终completion.json与报告按COMPLETION合同生成。不能提前生成一个success示例让goal误判产品完成。
