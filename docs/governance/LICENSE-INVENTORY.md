# 许可与依赖清单（实测记录，不是许可决定）

本文件只记录**当前仓库实际状态**与技术核验结果。许可决定本身在 [LICENSING.md](LICENSING.md)；本清单不改变已采用的许可，也不新增授权。本清单在 2026-11-01 按仓库实际内容重测，旧版把三份治理文档当作"PROPOSED 未生效"的描述已经过时——[LICENSING.md](LICENSING.md) 的状态是**已采用**，并附有采纳记录与贡献者确认。

## 项目自身许可状态（2026-11-01 实测）

| 位置 | 实际内容 |
| --- | --- |
| `Cargo.toml` `[workspace.package] license` | `AGPL-3.0-or-later` |
| `LICENSE` | GNU AGPL **version 3** 正文（FSF 标准文本，无附加声明） |
| `NOTICE` / `COPYING` | 不存在 |
| 源文件头 | 没有逐文件许可头（`SPDX-License-Identifier` 命中 0） |
| 24 个 workspace 包 | 全部为 `license.workspace = true`，继承 `AGPL-3.0-or-later` |

两点需要如实记住：

- 采纳记录（issue #54、PR #61/#64、贡献者确认链接与时间戳）在 [LICENSING.md](LICENSING.md) 与 [CONTRIBUTOR_LICENSE.md](CONTRIBUTOR_LICENSE.md)。owner 本人保留闭源与商业双许可，边界与原文引用一并保留。
- `or-later` 的选择目前只写在 Cargo 元数据与许可决定里；`LICENSE` 正文是未经改动的 FSF 标准 AGPLv3 文本，本身不含"or any later version"声明，仓库也没有 NOTICE 说明这一选择。发布二进制时是否补写许可通知是许可决定，本清单只指出该事实，不擅自改动 `LICENSE`。

## 第三方依赖

`Cargo.lock` 当前锁定 **426 个包**。逐包 SPDX 声明可用同一命令重新生成：

```sh
cargo metadata --offline --format-version 1 | jq -r '.packages[].license' | sort | uniq -c | sort -rn
```

2026-10-25 的一次同命令测量显示：除本项目自身外没有 GPL-only 或 AGPL 第三方依赖——两个带 GPL 字样的依赖都是双许可（可选宽松分支）：`ittapi` / `ittapi-sys`（`GPL-2.0-only OR BSD-3-Clause`，选 BSD）、`r-efi`（`MIT OR Apache-2.0 OR LGPL-2.1-or-later`，选 MIT/Apache）。

指定插件主干 `cordis-core` 0.6.1 为 **MIT**（git 依赖，rev `9a8a49fa459991ee29da73fb42ea9324cee9b680`），与 [LICENSING.md](LICENSING.md) 的核验一致；不能因为本仓库选择强 copyleft 就改写上游许可。

边界：上面的 SPDX 分布来自 registry 索引与 `Cargo.lock` 的声明字段，**不是**对每个 crate 内嵌 LICENSE 正文的逐份比对；发布二进制时分发义务仍需按实际打包内容复核。引入新依赖后必须重新生成本表。

## 随 UI 打包的第三方许可正文

`crates/shenko-ui/src/static/licenses/` 保留 `REACT-LICENSE`、`REACT-DOM-LICENSE`、`SHADCN-NOTICES.md`、`PROTO-UI-LICENSE`。这些是实际随资源分发的许可文本，改动 UI 依赖时必须同步更新，不能用 Cargo 元数据代替。

## 贡献与签署现状（2026-11-01 实测）

- 仓库共 **530 个 commit**，其中 **294** 个带 `Signed-off-by`；最近 200 个 commit 中 **160** 个带。历史贡献未必逐个签署，因此不能声称"全部贡献均已按 DCO 签署"。
- [DCO.md](DCO.md) 是**现行的轻量来源声明**：提交即确认来源与提交权，不要求 GPG 签名，也不设自动检查工作流或 CLA 门禁。
- 商业双许可需要贡献者另行同意 [CONTRIBUTOR_LICENSE.md](CONTRIBUTOR_LICENSE.md)；DCO 不自动授予闭源或商业再许可权。[LICENSING.md](LICENSING.md) 记录了已确认的贡献者确认原文与时间戳，未确认的贡献不自动追溯为商业授权。

## 不做什么

- 不修改 `LICENSE`、`Cargo.toml` 许可字段或任何贡献者授权文本；
- 不新增自动 DCO/CLA 门禁，也不把已采用的许可退回提案状态；
- 不追溯改写历史提交的署名或作者。
