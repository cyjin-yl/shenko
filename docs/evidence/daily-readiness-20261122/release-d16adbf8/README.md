# d16adbf8：已关闭的 release 构建原件

源码候选 **`d16adbf89c9f79dfce8ef90d77843412bcda85bf`** 的实际 release 构建
于 **2026-11-21 23:50:15 UTC** 完成，Cargo 和宿主 runner 均退出 0。
本目录只保全这次已关闭构建的原小文件、日志及精确执行脚本，不包含运行中的
模型安装、90 MB 二进制或 CAS，也不改变任何模型旅程的判定。
[原构建 manifest](manifest.json) · [派生摘要](summary.json)

## 产物与实际命令

原 runner 在构建成功后复制二进制，并对实际复制的字节计算 SHA256：

- 路径：`/tank/shared/projects/shenko/build/daily-readiness-20261122/shenko-release-d16adbf8`
- 长度：**90,351,680 bytes**。
- SHA256：`d44703d84ff02a4235005e54c935ddeb2b6dc78a4dbe4184e981c293f1daccce`。
- 命令：`cargo build --offline --locked --release -p shenko -j4`。

这些数值来自原构建 manifest。此次保全没有复制或再次读取整个二进制来独立
重算摘要。实际项目容器 image 是
`sha256:28df77cc6beee4f88ee1f57922190dcb02414d78d284e8b0f91859e18728e565`；
原工具链记录为 Rust/Cargo 1.98.1、Node v24.22.1。
[原容器 runner](runner.sh) · [原宿主 runner](host/runner.sh) ·
[Rust](rustc.txt) · [Cargo](cargo.txt) · [Node](node.txt)

宿主 runner 先验证源码 bundle、取得共享锁、确认实际 checkout 干净且 HEAD
精确相同，再校验容器 image 并调用容器脚本。容器脚本另持其本地锁。源码接线和
静态文件门通过之后才执行 release 命令；没有借这次保全重新编译、修改 checkout
或改变任何运行中的观察。

## 时间粒度分别保留

| 原观测 | 数值 | 含义 |
| --- | --- | --- |
| Cargo 日志 | `Finished release profile [optimized] target(s) in 7m 58s` | 原 Cargo 完成行；原标点与格式见日志 |
| build started / finished | 23:42:17 → 23:50:15 UTC，差 478 秒 | 秒级文件时间跨度 |
| host started / finished | 23:42:15 → 23:50:17 UTC，差 482 秒 | 宿主调用容器阶段的秒级时间跨度 |
| Remote process PID 2532083 | 482.80 秒，exit 0 | 工具对整个实际进程报告的 wall time |

不把这些不同边界和精度的数字混成一项精确计时。
[原 build 日志](build.log.gz) · [原开始](started.txt) · [原结束](finished.txt) ·
[宿主开始](host/started.txt) · [宿主结束](host/finished.txt) ·
[原工具完成回执](host/completion.tool.json)

日志含 warnings，全部保留；本证据没有把所有 warnings 称作此前已有，也没有
仅凭没有 error 字样宣布成功。实际 [Cargo exit](exit.txt)、[host exit](host/exit)
和原完成行共同证明这次构建退出 0。

## 前置门与复用边界

原 [preflight.json](preflight.json) 保留以下实际检查：

- 972e 的 12 个通过测试来自原 `scoped-972e16ef`；原该轮 native-chain 失败仍保留。
- 唯一 fixture 断言修正后的 native-chain 在 4a 实际重跑 1 项并通过。
- 从 4a 到 d16 的全部变更路径属于 UI 与文档；本轮 Rust 业务源码没有继续变化。
- 12 个实际 static payload 的成员、长度、SHA256 与 manifest 全部吻合；manifest
  SHA256 为 `134521c95f3676cf5afdd9eeae55b44a8eff144d7fef95c7b39047940919cd89`。

**这次 build runner 没有新执行这 13 个测试。** 它先逐字节验证先前原日志及其
通过/失败记录，再复用 12@972e + 1@4a；没有将原 972e 的失败改成通过。
原 972e/4a 的完整独立证据分别由 `2050b28405bd98dc7c8da1e5ae248c0a9362d05c`
与 `f93832861706a1b07efc239819b4e14a09c939e5` 保全。

UI 的两次真实生成与 reducer baseline 对照另见
[ui-corrected-ef2256e1](../ui-corrected-ef2256e1/README.md)：原代码 0/1 失败、
候选同一批 51 项测试各运行两次通过，固定上游 42 包各真实构建一次，12 个产物
逐字节一致。该 UI 证据和这里的 Rust release 构建各自保留来源身份，不充当
最终浏览器、模型任务、重启或无重复效果的验收回执。

## 原件保全

本目录包含 14 份原文件：release 目录的全部 9 份原小文件，原 preflight、三份
host 时间/退出文件，以及精确宿主 runner；另保留工具返回的完成回执。`build.log`
仅编码为 mtime=0 gzip，原长度与 SHA256 同时记录。其他原文件逐字节复制，包含
原始换行和全部 warnings。导出期间逐文件记录大小、摘要和 mtime，确认读取
期间未变；没有替换原文件或补造结束时间。

[原文件索引](retention-index.json) 将每份副本映射到原绝对路径；
[目录完整性清单](file-manifest.json) 覆盖原件、派生摘要和本文。
`manifest.json` 始终是原二进制构建 manifest，没有被目录清单覆盖。
