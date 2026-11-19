# 第二次固定 Node-WASIX 源构建：上游绝对 header 链接失败

[实际 run 43945415290](https://github.com/cyjin-yl/shenko/actions/runs/43945415290) 的唯一 job `133573644956` 于 **2026-11-16 20:50:22 UTC** 完成，结果为 **failure**。编译越过了前轮失败的 OpenSSL 归档命令，在 V8 `builtins-typed-array.cc:17` 因 `wasi/wasi-simdutf-compat.h` 无法打开而结束。make 返回 2；没有得到本旅程所需的完整 Node module，原三段 cell、fs witness 和条件性 reopen 均未执行。

## 源码和原件身份

| 项目 | 实际记录 |
| --- | --- |
| Workflow / job / attempt | `43945415290` / `133573644956` / `1` |
| 请求的 PR head | `83bf391440adbe3e153b036ba8fd1ecfe5867202` |
| 实际 checkout | `2d275430b103fb0e13316ba51cc5b5c5fdafa262` |
| 实际 tree | `d695242eb0005a1d06256aab91e9e649f1cb8a86`，与请求 head 相同 |
| checkout 父提交 | main `84ae19404f2a17d259362c99091db408b7262e14` 和请求 head |
| Node 固定源码 | `Multi-V-VM/node-wasix32@20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f` |
| Node 原 tree | `1f91f029db924986b371c4b848fb65ffd080613d` |
| Artifact | `14272000277`，2,226,962 字节，121 个成员 |
| 原 ZIP SHA256 | `8048e747580e94197b730e1ebec321088dd45171a0ca1f9edb5076379f895ba5` |

[原始完整 ZIP](original.zip) 保留全部 121 个成员：完整编译 stdout/stderr、574 份资源样本、所有执行过的工具和下载记录、配置和生成规则、原源码清单、builder/runner、driver、原 framed inputs、失败与 cleanup 观察。下载后独立计算的 ZIP SHA256 与 [GitHub artifact 来源](artifact-source.json) 一致，逐成员 CRC、大小、SHA256 及内部 `files.json` 的 120 项全部校验通过；清单自身是第 121 项。详见 [逐成员校验](zip-verification.json)。原 ZIP 和完整日志没有被截断或重写。

[完整 job 日志](job.log)、[run API](run.tool.json)、[jobs API](jobs.tool.json)、[artifacts API](artifacts.tool.json) 和 [实际 checkout commit API](checkout-commit.tool.json) 保留原工具响应。通过固定 checkout 的 [builder](build-source.tool.json)、[runner](runner-source.tool.json)、[既有 patch](patch-source.tool.json) 和 [workflow](workflow-source.tool.json) 原文独立核 Git blob；前三者与 artifact 对应成员逐字节相同。[选定原件](selected-originals/manifest.json) 仅为方便阅读的原字节副本，完整范围以 ZIP 为准。

## 确切失败与已执行阶段

`original.zip!compile.stderr` 第 **255367** 行是 `fatal error: 'wasi/wasi-simdutf-compat.h' file not found`，第 255371 行保留完整失败 clang 命令，第 255372 行报告 `builtins-typed-array.o` 目标 Error 1。该命令已经带有 `-I../deps/v8/include`，因此不能把这次失败直接归为缺少这个 include 参数。[observation.json](observation.json) 保存有源文件 hash 的原错误行、完整命令与阶段摘要。

此前具名补丁仅把固定 GYP 中 WASI/WASM 归档规则接到既有逐对象 response-file 路径。原 patch SHA256 为 `93970b459d28ad3ac6af9c6446bc175220b4ea3b2f9ab29f5330710454d5fee9`。本轮 `compile.stdout` 第 2959 行确实执行 `wasixar crsT .../libopenssl.a @.../libopenssl.a.ar-file-list`，后续进入 V8 编译；原生成规则及 response file 仍保存在 ZIP。这是越过前轮具体命令失败的证据，并不代表整个构建成功。

前轮 [run 43922989826](https://github.com/cyjin-yl/shenko/actions/runs/43922989826) 的 `make: printf: Argument list too long` 仍保留在 [既有独立原件归档](https://github.com/cyjin-yl/shenko/blob/74e0209bc17da201a4dfab13c08bb755c2f614ca/docs/evidence/node-wasix-source-hosted-43922989826/README.md)，本条没有改写它。更早预构建 module 的 `BigIntConstructor` 运行失败同样不因本次部分编译而消失。

## 固定上游树的 header 来源

原 `source-files.stdout` 是完整 `git ls-tree -rlz` 输出。其中 `deps/v8/include/wasi/wasi-simdutf-compat.h` 的 mode 是 `120000`，blob 为 `f7e59ed84434ac2100e830dfc652203f1ccdad48`。后续只读读取的 [原链接 blob](blob-f7e59ed84434ac2100e830dfc652203f1ccdad48) 是 **81 字节绝对链接**，指向 `/home/victoryang00/node-wasix32/deps/v8/third_party/simdutf/simdutf-wasi-compat.h`。这不是本轮 checkout 的目录。

它所指的同树 [真实 header](blob-e855e0ab27eecb80fabb1c05d1abb08caaf3c4d8) 已存在，blob `e855e0ab27eecb80fabb1c05d1abb08caaf3c4d8`，1449 字节；实际发起 include 的 [原 V8 文件](blob-bd6e8c79587d83d5c6eaac604f4acd4faf9b105d) 也与固定树 blob 一致。这三个后续读取的原文都独立重算了 Git blob SHA，并在 `observation.json` 中记录 SHA256；它们是上游来源核验，不冒充 runner 当时额外保留的文件。

同树 **8 个 mode 120000** 的 blob 均只读核验，只有上述一个为绝对路径。其他 7 个是相对链接；其中 c-ares 的 `config/ares_config.h` 不在静态 tree 的相应位置，此次核验不推断其生成阶段，也不改它。完整路径、目标、blob、静态解析结果均在 `observation.json`。因此可审的下一步是仅将已查实的绝对链接改为同树相对目标 `../../third_party/simdutf/simdutf-wasi-compat.h`，保留真实 header 原文并记录具名 patch 来源。**该修复不在本归档提交中，也没有后续编译或 guest 通过证据。**

## 资源、cleanup 和验证边界

编译从 **20:02:29.095182 UTC** 到 **20:50:16.890832 UTC**，经过 **2867.795 秒**。原观察为 `exit_code=2`、`stopped=null`、`leader_reaped=true`、`group_remaining=[]`，summary 的 `owned_groups_settled=true`。没有记录到资源监视器触发：采样 group RSS 峰值为 **3,361,767,424 字节**，最小可见有效内存余量为 **12,280,815,616 字节**，最小空盘为 **92,715,462,656 字节**。原 10 GiB RSS、24 GiB 单进程地址空间、8 GiB 空盘余量和约两小时总构建预算没有为本次诊断调高。采样与可见 cgroup 范围不能证明不可见环境；这里仅报告实际未触发 guard 和编译器的明确错误。

[只读复核脚本](inspect_archive.py) 的最终完整 [stdout](inspection.stdout) / [stderr](inspection.stderr) 保留 exit 0 结果，核对 ZIP、内部清单、源码绑定及全部符号链接。第一次复核把 GitHub 分行 base64 当作无空白 base64，exit 1；[初版脚本](inspect_archive-initial.py) 与 [原错误](inspection-initial.stderr) 一并保留，最终只修正标准 base64 换行处理，没有修改任何原件。此检查不执行 builder、编译器或 guest。`git diff --check` 返回 2，全部 7 处空白告警来自未改写的原 job 日志；[完整检查输出](diff-check.stdout) 保留，不裁剪原文以取得形式通过。没有新增产品场景；Rust、文档 parser 和新的源码构建均未在本次归档运行。

Node 的正式安装、能力广告、配额、恢复和完整验收仍未取得，本条也没有 Bun 的运行证据。完整文件范围和 hash 见 [归档索引](archive-index.json)。
