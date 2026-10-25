# Shenko 许可（AGPL-3.0-or-later，2026-10-25 正式采用）

状态：**已采用**。根目录 `LICENSE` 为 AGPL-3.0 正文，`Cargo.toml` 工作区许可元数据为 `AGPL-3.0-or-later`。owner **本人 `cyjin-yl`** 保留闭源、商用及另行商业双许可的权利（机制见 `CONTRIBUTOR_LICENSE.md`）。

## 目标

1. 社区版 Shenko 持续开源，避免把公共贡献变成闭源竞争产品或只对付费托管服务开放的衍生品。
2. owner 本人保留闭源、商用及商业双许可的可能。
3. 贡献者保留原作者身份与 Git 提交归属；授权边界清晰，不用 DCO 冒充商业授权，也不把第三方代码据为己有。

## 现行方案

- **社区主许可：AGPL-3.0-or-later**。公开网络服务的使用者也应能获得对应修改后的源码；它不禁止企业内部私有使用。
- **owner 个人商业双许可**：贡献者对其有权贡献的 Shenko 部分，向 `cyjin-yl` 授予非独占再许可（见 `CONTRIBUTOR_LICENSE.md`）。
- **DCO**：只证明贡献来源与提交权，不自动授予闭源或商业再许可权（见 `DCO.md`）。

选择 `or-later` 便于未来评估许可证升级；这是 owner 的明确决定，不再作为待办。

## 采用记录（2026-10-25）

- 2026-10-25 01:25 UTC：在 issue #54 征求当时唯一外部贡献者 `shwgdysjsvg` 对 AGPL＋owner 双许可的意见。
- 2026-10-25 04:45 UTC：提案文档经 PR #61 合入 main，明确标注 PROPOSED 并再次邀请讨论。
- 征求意见超过 3 小时后，主控按 owner 已明确的决定采用本方案；PR #64 补齐 LICENSE 与包元数据。接管开发决策不冒充另一贡献者作出授权，其明确确认记录如下。
- **既有贡献**：此前仓库仅有 `Apache-2.0` 元数据、无完整 LICENSE 正文。Apache-2.0 贡献可在保留版权与许可声明的前提下并入 AGPL-3.0 项目；所有原作者、提交与来源 PR 均保留，未改写历史。
- **贡献者明确授权**：`shwgdysjsvg` 于 **2026-10-25 12:20:53 UTC** 在 [#54 原始确认](https://github.com/cyjin-yl/shenko/issues/54#issuecomment-6397563508) 写明：“我同意 docs/governance/CONTRIBUTOR_LICENSE.md 的条款，并仅就我有权贡献给 Shenko 的部分授予上述非独占再许可。”所指协议位于当时 main `d9a6ebcf64`，Git blob 为 `f9d9bf1a8e0ff364afe1a21c81d3c75448ae4d98`。主控已[回复确认](https://github.com/cyjin-yl/shenko/issues/54#issuecomment-6397601162)。授权仅覆盖其有权贡献给 Shenko 的部分，不扩展到第三方作品；原作者、原始 commits 和来源 PR 保留。其他贡献者仍各自按 `CONTRIBUTOR_LICENSE.md` 与 `DCO.md` 确认，不能沿用他人的授权。
- 依赖核查：`cordis-rs` 为 **MIT**，不改写上游许可；第三方依赖与前端资源（字体、组件、构建产物）各自许可与 NOTICE 保留，持续在引入时核对。

## 权利边界

- owner 的再许可主体是 **`cyjin-yl` 本人**，不默认转移给未来公司、基金会或任何 agent。
- 贡献者只授权其实际有权提交的 Shenko 部分；第三方依赖仍按各自许可证处理。
- 贡献者确认记录：issue #54 及各 PR 的提交确认；不同意商业再许可的贡献仍可按 AGPL 路径合入，不阻塞开发。
