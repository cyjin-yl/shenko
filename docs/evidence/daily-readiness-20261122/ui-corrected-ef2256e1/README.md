# Corrected part：原 reducer 复现与实际前端生成

本轮修复源码为 `ef2256e1a420a739b9ddccb18f57af96ff2979b9`，直接父为
`4a5b88aa58e5f5c61200066fa8d37b0c6fb4dfcb`。实际执行时间是
**2026-11-21 23:29:08–23:32:33 UTC**，目录继续沿用本轮的
`daily-readiness-20261122` 名称。原代码的有界复现失败，修复后的完整前端检查和
两次独立生成通过；这不是新的真实模型或浏览器旅程通过记录。
[机器摘要](summary.json) · [原宿主日志](host/host.log.gz) · [原容器日志](container/container.log.gz)

## 原缺口与修复范围

先前 0c 第三次真实旅程收到完整 S1 正文，其 part_id 为
`text/0@b1abb45e-e88b-4485-8fdd-dc3a65aced3e/389`，message_id/step_id 均为
`449d0aa0-1ded-457b-bb0a-82bca424aacc`。最终 DOM 却仍保留同一消息的旧
`text/0` 流，`stable_fact=false`。原件保持在
[第三次浏览器记录](../final-0c6b013e/attempt-003/browser/final.dom.json) 与
[原批次汇总](../final-0c6b013e/batch-summary.json)，没有改写原 0/3 结果。

修复落实既有 Web §1.1 corrected 替换契约：精确匹配
`text/0@${commit_ref}` 与同一 `message_id=step_id`，替换未提交的原 `text/0`
视图或同一提交引用的 base，清理对应旧流待补分片。先收到旧流、先收到 corrected、
迟到分片及重放，都保留实际 corrected part_id 和全量正式正文。其他消息、part
与不同提交引用保留，修正 part 不借用旧流的 stream/request/attempt 身份。

没有修改 ledger 原流、服务端身份、提示词、parser、driver 或 §5.2 的静默轮次
归类。尤其原 0c003 的最后 selected receipt 指向后续请求，而不是上述 corrected
step；修复本身不能证明该正文会变成直接回复气泡，也不会删除另一条被 R0 拒绝的
消息。最终候选仍须分别观察收到绑定帧与实际 DOM 展示。

## 先保留原代码的实际失败

baseline 是容器内独立真实 Git checkout，HEAD 精确为父提交 4a；唯一工作树差异
是从 ef 原样复制的既有 `live-controller.test.mjs`。原生产 reducer 字节不变，
候选与 baseline 使用同一测试文件。它通过真实 esbuild 编译和 live controller
运行，没有替换 reducer 或直接构造预期结果。
[原输入绑定及命令](baseline/receipt.json) · [唯一测试差异](baseline/fixture.patch.gz)

扩展原 `a streamed prefix is not labelled committed and is replaced by the
committed text` 轨迹，新增两种 corrected 到达顺序，同时检查迟到旧分片、缺口、
重放，以及其他消息/part/提交引用的隔离；没有新增 test 函数。

baseline 实际结果是 **1 个既有测试失败，0 通过、0 跳过、0 取消**。联合断言
前已收集两种顺序：旧流先到时，corrected 后仍有未提交且 gap=true 的旧正文；
corrected 先到时，迟到旧流和 base 重放又留下第二份正文。原 TAP 保留实际值、
期望值和堆栈，SHA256 为
`deb72df5cab5a75802d9e7e2b8c86aba2578b384855ebcdf4dbec5ad424df224`。
[原失败 TAP](baseline/replacement.log.gz)

## 修复后的两次完整生成

实际项目 devcontainer image 为
`sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565`，
Node `v24.22.1`、pnpm `10.32.1`，上游仍固定
`795b1b4958333cf904547e484acb4de9d4b3ee80`。使用宿主 cargo.lock 和容器内
独立锁串行执行。源码、Git 元数据、依赖和输出在新的持久私有目录中；旧 r4
生成目录、宿主原仓库与 live worktree 没有修改。
[实际容器脚本](container-build-web.sh) · [实际宿主脚本](host-build-web.sh)

| 检查 | Producer | 原 `web_assets.py` fresh rebuild |
| --- | --- | --- |
| 固定上游 public closure | 实际构建 42 包 | 新 checkout 再实际构建 42 包 |
| 前端类型检查 | 通过 | 通过 |
| 既有前端测试 | 51 通过，0 失败/跳过/取消 | 同一 51 项通过，0 失败/跳过/取消 |
| corrected 扩展轨迹 | 实际通过 | 实际通过 |
| 静态文件 | 12 个完整 payload | 同一成员集合与逐字节内容 |
| 源码保全 | 53 个 web 输入、13,502 个 upstream 输入前后相同 | fresh verifier 检查原输入与独立输入均未变 |
| compiler / Prototype 来源 | 实际 source、bundle、生成源码与 provenance 校验 | 稳定 manifest 和全部相关摘要一致，`errors=[]` |

这里是同一 51 个测试执行两次，不是 102 个不同测试，也不是完整 compiler Nightly。
[Producer 测试原日志](generation/web-tests.log.gz) ·
[独立测试原日志](rebuild/web-tests.log.gz) ·
[原比较回执](rebuild/comparison.json) ·
[输入来源摘要](generation/input-summary.json)

实际 producer manifest 为 8,978 bytes，SHA256
`134521c95f3676cf5afdd9eeae55b44a8eff144d7fef95c7b39047940919cd89`；
它原样进入 checked-in static。独立 manifest 的 SHA256 为
`36f2ea2f562b096e00dfc639278068a9385aa984d7190492464c17613a0128a3`。
两者只有既有 verifier 允许的运行时间、路径和实际 upstream-build receipt 身份
可不同；没有手工修改期望摘要。与父候选相比，12 个 payload 中只有 app.js 改变。
[Producer 原清单](producer-manifest.json) · [独立原清单](rebuilt-manifest.json)

## 保全、交还与验收边界

宿主 PID 2490502 自然退出 0，工具报告运行 204.46 秒；容器原 exit 文件也为 0。
两项锁随后释放。结束后实际读取宿主 live worktree，仍为 4a 且
`git status --porcelain` 为空。没有启动 Cargo、模型或新的 GitHub workflow。

取回的 delivery archive 为 416,862 bytes，SHA256
`4f8d4bd7de7cd8d3d2cf43f813dc7c7e5f8ef33e75ead35d4025301de64f6df4`。
原包位于宿主 `/tank/shared/projects/shenko/build/ui-corrected-ef2256e1/delivery.tar.gz`
和容器 `/var/idw-target/ui-corrected-ef2256e1/delivery.tar.gz`。七段传输重组后再次
核对整包摘要、每个静态 payload 以及本地 ef 的全部 web 输入，才复制产物。
完整原输入清单继续保留在容器该目录的 `generation/inputs-before.json`、
`generation/inputs-after.json` 与 `rebuild/inputs.json`；本目录保留其来源摘要、
实际脚本、工具原回执和原始日志，日志及原 fixture patch 仅作 mtime=0 gzip 编码。
[结束后的只读复核](postrun-verification.json) 另记录三份完整清单的长度/摘要，
并确认 baseline reducer/fixture 仍与开始时相同，producer 只改变两个静态文件。
[文件保留索引](retention-index.json) · [本目录摘要清单](manifest.json)

这些回执证明该窄修复的 reducer 行为和实际生成资产。真实参考模型的任务完成、
浏览器最终回答、停止/恢复和无重复效果，仍由最终固定候选的新旅程单独验收。
