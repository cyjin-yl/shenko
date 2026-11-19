# Node-WASIX：真实候选、两次本机失败与 hosted 复现入口

本切片关联 [Issue #19](https://github.com/cyjin-yl/shenko/issues/19)、[具名 REPL 与恢复合同](../../specs/repl.md)。它增加一个可执行的上游 runtime 观察入口，尚未安装 Shenko WASIX 后端，`node-wasm` 与 `bun-wasm` 的可用性保持原事实。**该入口提交时没有成功的 Node cell、checkpoint/reopen 或 hosted 运行结果；不计作 Bun、完整 R1/R2、S1/S2 或 27B 行为验收。** 后续 [首次 hosted 运行](../node-wasix-hosted-43916483602/README.md) 已取得具体的 runtime builtin 失败，原始本机记录保持不变。

## 两次原始观测

[完整小型原件包](original-local-evidence.zip) 为 60,145 字节，SHA256 为 `6dc5f0c0267027e6f5ee36cf32c45d99733fb464b068753ab213d357bc9d0289`。其中保留原脚本、driver、完整 framed input、stdout/stderr、逐秒资源样本、终止与退出结果、发布资产清单和哈希。包内 `evidence-files.json` 记录 43 个原件条目；大型 SDK executable/module 仅保留精确来源和哈希，没有重复纳入 Git。公开资源记录保留 cgroup/内存必要信息，省略无关私有 mount 路径。原件包在后续整理中没有重写。

| 本机尝试 | 唯一配置差别 | 实际结果 |
| --- | --- | --- |
| 首次 | 虚拟地址上限 8 GiB，RSS 观察门 1.5 GiB | 56.38 秒后 exit 1，Wasmer 报映射分配失败；RSS 观测峰值 1,280,180,224 字节 |
| 有依据的修正 | 虚拟地址上限改为 12 GiB，其他限制、输入和 driver 保持 | 55.54 秒时 RSS 达 1,613,180,928 字节，触发我们主动设置的 RSS 门；SIGTERM 后 exit -15 |

两个 owned process group 均已观察为空，编译 cache 没有文件。没有成功 cell 报告或 fs witness，条件性的 reopen 没有运行。第二次是实验预算主动停止，**没有观察到物理 OOM**。之后可见系统 MemAvailable 约 3.35–3.36 GiB，但执行 namespace 没有 cgroup mount，祖先 `memory.max/current/high` 不可核实，因此没有在本机继续提高预算。这个未知观测边界不能解释为 Node-WASIX 永久不支持。可按固定 hash 重取的 Wasmer CLI 暂存随后回收以释放共享磁盘；原观测没有删除，详见 [机器记录](local-observation.json)。

8 GiB 虚拟上限的问题有固定源码依据：Wasmer 静态 wasm32 映射预留 4 GiB 地址空间、4 GiB offset guard 和一页 64 KiB，单处需要 8,590,000,128 字节。相关实现为 [MemoryStyle](https://github.com/wasmerio/wasmer/blob/82ff099e082da586b38af79986abbb8b677f8baf/lib/types/src/memory.rs)、[BaseTunables](https://github.com/wasmerio/wasmer/blob/82ff099e082da586b38af79986abbb8b677f8baf/lib/compiler/src/engine/tunables.rs) 和 [memory mapping](https://github.com/wasmerio/wasmer/blob/82ff099e082da586b38af79986abbb8b677f8baf/lib/vm/src/memory.rs)。12 GiB 是针对这个已确认映射需求的诊断配置，不是内存使用成功保证。

## 固定资产与来源边界

| 输入 | 精确身份 |
| --- | --- |
| 官方 Wasmer v7.5.0 | [发布资产](https://github.com/wasmerio/wasmer/releases/download/v7.5.0/wasmer-linux-amd64.tar.gz)，302,721,336 字节；SHA256 `23f99818deaad93859f8bb8292af81f0c39f25aea19eaccfb72fc7c9e8a3fae9` |
| Wasmer executable | 219,153,104 字节；SHA256 `72c57ef6be45d500cb528c0a684b13159620ddfb4b50d3e429a3a352d0b3f116`；发布源提交 `82ff099e082da586b38af79986abbb8b677f8baf` |
| 第三方 Node-WASIX release asset | [node.wasm](https://github.com/Multi-V-VM/node-wasix32/releases/download/wasix32/node.wasm)，112,379,740 字节；SHA256 `3e0b4c9f9c258fef4ecf522bf86c42844d50368138abb86d5c2700775cf84a1d` |
| 原 Shenko JS driver | `crates/shenko-repl/src/drivers/js_cell_driver.mjs`；SHA256 `a9419c9aa0abbced7c69145806768153971d37121a8c20e63e0c623f0991f150`；本机来源 `ce18c5ec89163d19f07037e5e9489ef33c376fc9` |
| 原首轮 framed input | SHA256 `f1c895141dd1cbb589de36e6c93900da14b1f24f96ee24c2016afc303d43c0dc`；hosted 脚本启动时验证相等 |

Node-WASIX 是第三方 Node/V8 移植，不能表述为官方 Node 发布的 WASM binary。release tag 指向 `0d66fea37737024056983f35e84af3e8448ece08`，但 tag 中跟踪的 module 是另一份 100,589,868 字节对象；没有获得该 release asset 的源码构建 attestation。完整校验和证明的是发布资产身份。直到实际观察 `process.versions`，不能用仓库中的 Node 版本声明替代执行版本。

本机读取的模块 import section 有 83 个导入，来自 `env`、`wasi_snapshot_preview1`、`wasix_32v1` 与 `wasi`，包括共享 memory、thread-spawn、futex 和进程接口。该 ABI 不能通过 Shenko 现有 Preview 1 linker 直接运行。成功 standalone 观察之后，仍需真正的 WASIX backend、runtime/module 封印、资源与取消语义、依赖 generation 和恢复证据。native Node、QuickJS、Edge.js 或一个 CLI wrapper 都不能直接把 `node-wasm` 标记为可用。

## 如何执行同一旅程

[工作流](../../../.github/workflows/node-wasix-feasibility.yml) 仅接受手动 `workflow_dispatch` 或一次 `run-node-wasix` PR 标签事件；普通 push、无关标签不会重跑，后来的 opt-in 不取消正在运行的 owned cleanup。入口提交自身没有触发远端；随后唯一实际运行另见上面的证据链接。它在 Ubuntu 24.04 hosted runner 运行 [stdlib Python 脚本](../../../scripts/real_node_wasix_journey.py)，无需构建 Node/V8 或 Shenko，也没有使用宿主 Node 执行测量 cell。

脚本先核对实际 checkout SHA、原 driver 和完整首轮输入，再观察系统 MemAvailable、磁盘及当前 cgroup v2 到可见 mount 根的祖先链。没有匹配 mount、祖先被 mount root 隐藏，或非根祖先的实际内存上限/使用量不可读时，明确留下失败。真正的 cgroup 层级根没有 memory limit 文件是 Linux 的正常语义，不能误认为它有一个零额度；依据为 [Linux cgroup v2 memory interface](https://www.kernel.org/doc/html/v6.17/admin-guide/cgroup-v2.html)。观察范围始终只涵盖可见系统与祖先，不声称看到了 namespace 外部限制。

下载前和执行前均记录资源。运行预算取系统 MemAvailable 与每层有限 `memory.max/high - memory.current` 的最小值：保留至少 1 GiB 或该余量的四分之一，剩余作为 RSS 观察预算，向下对齐 64 MiB 并最高 6 GiB。不足以保留余量且给出 2 GiB 起始预算时，先记录拒绝。这比本机固定 1.5 GiB 门更符合实际 hosted 容量；任何未来资源拒绝仍只是该次配置的实际结果。磁盘保留至少 1 GiB，最多预留现有空间的五分之一或 4 GiB。编译固定单线程、禁 cache；没有同时落压缩包与全 SDK，只完整流式校验并选择提取 executable/许可证。

原旅程保持为两次条件性 invocation：

1. 第一轮两个 cell 输出真实 Node/V8 版本，计算 1,517 并以 exclusive create 写入/读取临时 fs witness，再保留 lexical binding 到 1,518，要求两份 `ok`/`state_verified` 报告与实际 checkpoint。
2. 只有上面全部通过才启动原定 reopen，恢复 JSON checkpoint 到 1,519，并核验原 fs witness 仍为 1,517；不重放先前写入。

每个 guest 保持原 180 秒 wall / 150 秒 CPU 预算、12 GiB 虚拟地址上限、16 MiB 单文件上限和无网络边界。环境完全替换，只有 owned guest 目录映射；runtime home/cache/temp 全归本次拥有，无 ambient credentials、host command mappings 或 registry 访问。seccomp 拒绝 socket/connect/bind/listen/accept/send 系统调用。RSS/余量检查是 50 ms 采样的观察门，不宣称为硬 cgroup RSS 限制；逐秒保存资源样本。收到停止、触发限制或失败后，TERM/KILL 与 leader/group 回收有界；未确认回收绝不作为通过或稳定 guest 快照。

`summary.json` 必须同时满足原 cell 判据、条件性 reopen、正常 exit、已回收 owned group 和空编译 cache。阶段、原 stdout/stderr、输入、guest 文件、完整实际 argv/clean env、资产校验、资源依据与最后文件哈希均进入 artifact；正常失败 exit 1，always 上传。公开 artifact 不带大型下载 binary 或无关私有 mount 信息。此入口没有给普通 CI 增加 scenario matrix，也没有调整已有验收门。

## Bun 仍保留独立缺口

一手源码审查中，[官方 Node configure](https://github.com/nodejs/node/blob/428dd8648909a321fa2c558bf01abebb17a74647/configure.py) 没有 wasm32/WASI 目标，因此这里采用单独固定的第三方候选。[官方 Bun 构建目标](https://github.com/oven-sh/bun/blob/237bba51e98a8b3d5b62d2d6c33853c2e83820a7/scripts/build/config.ts) 与 [JavaScriptCore 依赖选择](https://github.com/oven-sh/bun/blob/237bba51e98a8b3d5b62d2d6c33853c2e83820a7/scripts/build/deps/webkit.ts) 仍为 native OS 和 x64/aarch64；历史 [bun-wasm 包](https://github.com/oven-sh/bun/blob/a04817ce2b7f1a1e8b7cbf8af8f2c027ab072f1d/packages/bun-wasm/index.ts) 是 transformation/scanning 接口。这些有界查证没有找到完整 Bun runtime WASM artifact，不是取消 R080 或证明永远不可移植。
