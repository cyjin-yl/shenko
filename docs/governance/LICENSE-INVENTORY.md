# 许可与依赖清单（实测，不是许可决定）

本文件只记录**当前仓库实际状态**与技术核验结果，供 #54 的公开讨论使用。它不是新的许可
决定，也不把 `docs/governance/LICENSING.md`、`CONTRIBUTOR_LICENSE.md`、`DCO.md` 的
PROPOSED 提案当作已生效条款。

## 项目自身许可状态

| 位置 | 实际内容 |
| --- | --- |
| `Cargo.toml` `[workspace.package] license` | `AGPL-3.0-or-later` |
| `LICENSE` | GNU AGPL **version 3** 正文（FSF 标准文本，无附加声明） |
| `NOTICE` | 不存在 |
| 源文件头 | 没有逐文件许可头 |
| 21 个 workspace 包 | 全部继承 `AGPL-3.0-or-later` |

观察：元数据声明了 `or-later`，但 `LICENSE` 正文本身没有"or any later version"声明，仓库
也没有 NOTICE 说明该选择。若要把 `or-later` 作为正式授权，需要明确写进许可通知；这是
许可决定，本文件只指出当前不一致，不擅自采用。

## 第三方依赖（`cargo metadata --offline`，405 包）

| 声明（SPDX 原文） | 包数 |
| --- | ---: |
| `MIT OR Apache-2.0` | 184 |
| `MIT` | 53 |
| `Apache-2.0 WITH LLVM-exception` | 39 |
| `Apache-2.0 WITH LLVM-exception OR Apache-2.0 OR MIT` | 24 |
| `AGPL-3.0-or-later` | 21（本项目自身） |
| `Unicode-3.0` | 18 |
| `Apache-2.0 OR MIT` | 15 |
| `MIT/Apache-2.0` | 13 |
| `Unlicense OR MIT` | 4 |
| `Apache-2.0` | 4 |
| `BSD-3-Clause` | 3 |
| `Zlib` | 2 |
| 其余双/三选一声明 | 各 1–2 |

结论与边界：

- 除本项目自身外，**没有 GPL-only 或 AGPL 第三方依赖**。两个带 GPL 字样的依赖都是
  双许可，可选宽松分支：`ittapi` / `ittapi-sys`（`GPL-2.0-only OR BSD-3-Clause`，选
  BSD）、`r-efi`（`MIT OR Apache-2.0 OR LGPL-2.1-or-later`，选 MIT/Apache）。
- 指定插件主干 `cordis-core` 0.6.1 为 **MIT**（git 依赖，rev
  `9a8a49fa459991ee29da73fb42ea9324cee9b680`），与 #54 提案中的核验一致；不能因为本仓库
  选择强 copyleft 就改写上游许可。
- 该清单来自本地 registry 索引与 `Cargo.lock` 的声明字段，**不是**对每个 crate 内嵌
  LICENSE 正文的逐份比对；发布二进制时分发义务仍需按实际打包内容复核。
- 本表是聚合视图，逐包明细可用同一命令重新生成：
  `cargo metadata --offline --format-version 1` → `packages[].license`。

## 随 UI 打包的第三方许可正文

`crates/shenko-ui/src/static/licenses/` 保留 `REACT-LICENSE`、`REACT-DOM-LICENSE`、
`SHADCN-NOTICES.md`、`PROTO-UI-LICENSE`。这些是实际随资源分发的许可文本，改动 UI 依赖时
必须同步更新，不能用 Cargo 元数据代替。

## 贡献与签署现状（DCO）

- 205 个 commit 中，最近 200 条里只有 37 条带 `Signed-off-by`（本项目新增的提交使用
  `git commit --signoff`，历史贡献未必如此）。
- `docs/governance/DCO.md` 仍为 PROPOSED；仓库没有 DCO 检查工作流，也没有 CLA 自动化。
- 因此当前不能声称"全部贡献均已按 DCO 签署"，也不能据此推断任何再许可授权。提案里
  要求的贡献来源与签署边界仍需 owner 与贡献者按 #54 讨论后成文。

## 不做什么

- 不修改 `LICENSE`、`Cargo.toml` 许可字段或任何贡献者授权文本；
- 不把 PROPOSED 文档改成生效条款，也不新增自动 DCO/CLA 门禁；
- 不追溯改写历史提交的署名或作者。
