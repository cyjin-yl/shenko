# 执行环境中断、原件诊断与固定源码恢复

本目录记录 2026-11-16 的环境事实与恢复方法，不将源码恢复或只读分析当作产品运行验收。

## 中断与无法恢复的本地稿

原执行环境失联后，18:21 的新环境可以执行命令，但原源码、Rust 工具链、共享 target 和未提交草稿路径不在其中。旧 session 80554 没有取得终态，仍返回 transport/recovery timeout；不能称其通过。不可见的后继 E1 和容量草稿没有纳入任何已发布实现，后续从已推源码重新开发，具体只读清点见 unavailable-prior-draft.json。

新环境最初没有 Rust、Cargo、Docker、Podman、Clang、CMake 或文档 symbol/CSS parser。一次普通 mirror clone 因无凭据失败；没有绕过访问控制。有效访问继续通过已有 GitHub 连接完成。

## 诊断工作流原失败与修正

隔离分支 diagnostics/model5-artifact-43916477826-20261116 的第一次 [43927382049](https://github.com/cyjin-yl/shenko/actions/runs/43927382049) 在工作流验证阶段失败：job.env 使用了不允许的 runner.temp context。原页面 annotation、API、jobs=[]、artifacts=[] 都保留在 diagnostic-workflow/original。没有执行 Python、模型 ZIP 分析或产品，因此该失败不属于原模型运行。

修正提交 `283b5f37ce5ec2ea97bb7e7bc95504444db0c99e` 仅把该变量移到 step.env，并加入固定源恢复 job。精确 a9 inspector blob 没变。新 [43929782732](https://github.com/cyjin-yl/shenko/actions/runs/43929782732) 在 18:31:51–18:32:13 实际成功，两个 job 是只读原件分析和固定源码导出。它们没有运行 Cargo、模型或产品代码。完整 job 原文、本地语法验证、首次恢复小库探针失败及修正结果均保留。

原分析 ZIP artifact 14257049345（10,605,084 bytes，SHA-256 `150fb2c5015a8cff8d1be95a0fdb4145bea143604929a2bc4b85cb59acb8d5a0`）的 132 成员被独立核对，其内嵌原模型 ZIP 仍为 ab362c50…。完整模型原 ZIP 另见[第五轮原件](../real-single-node-model5-43916477826/README.md)，不重复存放相同大包。

## 源码恢复保留原 Git 身份

源码 artifact 14257518420 为 54,294,057 bytes，独立 SHA-256：

`76fcfd1dc2939f1b56786f8c87349dda2ccbca2ad53ed02e6ddd159312821c36`

其 17 个文件全部经过 CRC／manifest SHA 校验。导出的完整 source archive 覆盖 2,811 个实际 Git blob，另有原 commit／tree 对象 pack、原 commit 文本、父提交身份和明确 shallow 边界。没有导出 checkout 配置、环境令牌或凭据。

18:34:52 在空仓库中导入 pack，运行 index-pack／fsck 并 checkout，恢复出：

- commit `7905e8413b96015d96ea301bd1f3b6937dcfff90`
- tree `241909742fd73225b08b88fea967197a74f65334`
- clean 工作区；原父提交 ID 保留，但其完整历史对象不在本地 shallow 包中

没有用重写的根提交冒充原提交。source-recovery 保留原 metadata、完整命令结果与哈希；54 MB 的源码 archive／pack 不在此重复存放，因为完整源与真实历史已在原 Git 提交中，原 artifact 来源及精确摘要仍可查。

## main 整合与本地检查限制

PR303 已合入 main，随后 PR312 将规范引用从 subject 的注释搬入 symbol trace。root 将实际 main `84ae19404f2a17d259362c99091db408b7262e14` 的五文件变化整合到 7905，loop_runner 用精确 e5 base 作三方合并，保留原 schema14 等运行代码。实际 Rust diff 全为注释。

正常 merge commit `c7b6d542e6667da15bbd1f46c2daffe57a0337b8` 的父提交为 7905 与 84ae，tree 为 `e9463588cbdfd4db98e0f2ecddf9999fe526a304`。逐 blob／整个 tree 的远端身份与本地一致。导入原 commit 字节时先错误假定 UTC offset 为 +0000，身份校验拒绝；随后保留 API 时间对应的真实 +0800 author/committer offset，重建字节的 SHA 精确等于原 commit，再导入并明确 shallow 边界。未伪造 Git 身份。

本地 check_docs 实际退出 1：1,945 个 symbol parser 与 3 个 CSS parser 缺依赖报告。完整 stdout／stderr 保留在 main-integration，未添加 fallback 或放宽 gate。这不是文档检查通过；整合后的候选还需要原 CI。

恢复、只读诊断、source hash 与 metadata 检查均不表示 27B、S2、多节点完整语义或长期物理运行完成。
