# 共享 follow-tail 接续：当前主线与真实前端产物

日期：2026-11-22。范围仍是现有 Inspect / IM 阅读表面，接续原始 `c4f3c108` / `b257cc39`；[原记录](proto-follow-tail-20261122.md)保留原 v5、失败与责任边界。本轮整合 Shenko main `2c778c1832a71f68b6323c86af6c5fa9b1314b85`，保留其 timeline reducer 的已提交正文修正和其他主线改动。

## 当前 pin 与产物

当前 Proto-UI pin 为 [PR #1723](https://github.com/Proto-UI/Proto-UI/pull/1723) 的 `ad39435aeb3589d3bc52de3cbd1f4cdd375cec68`，包含 Proto-UI main `39ca4f7729cde9a3ebb3abfd4fd987d9db57e4de` 与共享显式 pause。上一 Shenko main 的 pin `8fcde0e1e522762969e89c0343a78179edb8c89e` 是其祖先；其原 Nightly 修正与失败记录继续保留。新的 pin 仍是配套 PR 候选依赖，不是已发布 release。

通过原公开包 builder 顺序构建 41 个 Runtime / family 依赖包，再构建 CLI，一个共 42 包的闭包。两段原始 JSON 和[组合回执](proto-follow-tail-continuation-20261122/s7-public-build-complete.json)分别记录实际过程；没有把第一段 41 包称为单次 42 包运行。依赖链接都解析到本轮隔离的 Proto-UI checkout。随后运行原 `build.mjs`，生产前端包含 12 个散列资产；浏览器图仍拒绝上游源码路径，两个 Shenko Prototype 仍由私有 Compiler 在构建期生成。

生成的 `app.js` 和 `build-manifest.json` 冲突通过真实重建解决，没有从旧候选挑选产物冒充当前构建。所有源文件与 manifest 的 SHA-256 见[本轮验证回执](proto-follow-tail-continuation-20261122/validation.json)。

## 实际验证与仍缺的观察

| 验证 | 结果 | 证据边界 |
| --- | --- | --- |
| 42 个公开包 | 通过 | 41 + 1 两段原 builder 回执，实际源码同一 pin |
| 生产前端构建 | 通过，12 个散列资产 | 保留上游 duplicate-case 警告；不据此宣称无警告 |
| TypeScript | exit 0，无诊断 | 在所有构建完成后串行运行 |
| 现有 UI 单测 | 51/51，0 skip | 没有新增用例数量或放宽原断言 |
| 文档完整性 | errors 为空 | 首次因缺少 tree-sitter 解析器失败；安装仓库锁定 requirements 后由原 checker 通过 |
| 原真实浏览器 v6 | 四条旅程 / 54 checks 通过 | 仍绑定原 Shenko `b257cc39` / Proto-UI `8ec1427`；本轮只独立复核原件与 11 个 source/built hash |

原 v6 的 public build、截图、全部检查和边界已恢复到 [Proto-UI 的完整回执](https://github.com/Proto-UI/Proto-UI/blob/ad39435aeb3589d3bc52de3cbd1f4cdd375cec68/internal/records/2026-11-22-sprint-7-harness.md)。它运行真实生产前端与确定性 HTTP/WS 读事实，原生 wheel / keyboard 与显式 DOM Range 选区仍分别标记。它没有证明本轮新 pin 的浏览器通过、物理输入、辅助技术、backend/model、Rust 内嵌二进制或生产部署。

当前 head 的原生浏览器阅读旅程、独立干净 checkout 的资产重建及实际 PR CI 仍需自己的终态。当前本地 Chromium 的 process-singleton socket 不可用，既有远端浏览器 executor 离线；没有修改现有 automatic CI 的触发、门禁或验收条件。重现命令继续使用原记录中的 `scripts/web_reading_journey.mjs` 和已发布精确 pin。
