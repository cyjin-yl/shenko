# 环境恢复后的原 ZIP 补证

上级 README 保留了 51a CI 失败时的真实取证状态：当时已取得 API 引用，但执行环境失联，尚未独立校验 ZIP。这一子目录追加恢复后的事实，不覆盖旧记录，也不把失败改成通过。

2026-11-16 18:28 UTC，在新的执行环境中从原 artifact 下载并独立核对：

| 原件 | SHA-256 | 实际完整校验 |
| --- | --- | --- |
| rust-original.zip，artifact 14256045836，8,305,765 bytes | `2a8609e899f1858a4d27ab4fc4c63d5f8dda90c909b0fb31804f4ddd42e762b6` | 1,836 成员全部 CRC／SHA-256 |
| clippy-original.zip，artifact 14254808026，2,768,629 bytes | `f7292c315d1aa5bfe400c8a4ecabe922a4630823e25efe9855a82902d6c76523` | 18 成员全部 CRC／SHA-256 |

两个 ZIP 按原字节归档。Clippy 原 base.json 绑定 e5fbf732 baseline 和 61a81357 candidate；原 ratchet、逐成员清单与验证记录也保留。此补证不改变该轮 E1 在旧 decision schema setup 失败、SSH 和九窗口尚未到达的结论。
