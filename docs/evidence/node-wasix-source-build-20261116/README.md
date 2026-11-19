# 固定 Node 源码的 WASIX 构建入口

此增量提供一个**尚未运行的 hosted 源码构建候选**，继续调查 [首次 hosted Node-WASIX 的真实失败](../node-wasix-hosted-43916483602/README.md)。那次模块明确报告 `BigIntConstructor` 未实现；两次本机失败和该次 hosted 原件均保留。本增量没有重跑旧 release module，没有下载或执行仓库中另一份旧 `node.wasm`，没有改写原 driver 的 u64 framing。

固定 main 源码中的 `TryFallbackJSEntryBuiltin` 已实现 `BigIntConstructor`，但现成模块没有与该修复绑定的构建证明。新入口从精确源码和已校验工具链生成新模块，保留构建记录后，复用原来的 finite cell／fs／checkpoint／reopen 旅程。**源码编译、standalone 运行、Shenko 已安装能力与正式验收是分别记录的事实。** `node-wasm`／`bun-wasm` availability、R1/R2、完整 S1–S5 与 27B 行为基准均未因此改变。

## 实际源码依据

Node/V8 移植来自第三方仓库 [Multi-V-VM/node-wasix32](https://github.com/Multi-V-VM/node-wasix32/tree/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f)，固定 commit `20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f`，tree `1f91f029db924986b371c4b848fb65ffd080613d`。不是官方 Node 发布的 WASM 工件。

本次只读检查得到以下具体入口：

- [configure.py](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/configure.py) 接受 `wasm32`／`wasi`；第 1473、1480–1488、1505–1518 行分别设置 wasm32 builtin 标记、独立 host toolset，以及 cross compilation 下禁用 Node snapshot／code cache 的既有规则。
- [common.gypi](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/common.gypi) 第 288–298 行仅给 target 加 WASI shim 定义；[node.gyp](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/node.gyp) 第 1415–1434 行将 `node_js2c` 和其依赖列为 host 工具。原 Node/V8 C++ 源码生成器可以在宿主运行；测量 cell 仍只交给构建出的 WASM 模块。
- [v8.gyp](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/tools/v8_gypfiles/v8.gyp) 第 411–415 行包含 wasm32 builtin 源，第 561–568 行移除该 target 不适用的 `mksnapshot` 依赖。没有将旧 root-level `.wasm` 文件复制成输出的步骤。
- [BigInt handler](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/deps/v8/src/builtins/wasm32/builtins-wasm32.cc) 从第 18157 行处理该 constructor；其来源与旧 release 的差异保存在前一份失败证据中。
- 上游 [增量构建说明](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/docs/superpowers/plans/2026-09-08-wasm32-ignition-loop-aot.md) 只给出既有 `out/` 下的 `make`。本入口补齐 fresh checkout、configure、工具链身份和输出记录，**不把该文档当成干净构建已成功的证据**。

只读 Git tree 元数据包含 41,100 个 blob、总计 648,431,593 字节，其中 root-level 旧 `.wasm` 文件占 223,451,820 字节；完整 tree 没有 submodule，顶层 Git tree 字节按对象规则重算为上述 tree hash。源码压缩包 HEAD 没有给出 `Content-Length`。新入口使用固定 commit 的浅 Git fetch，并记录实际 Git 目录／tracked blob 大小；这些磁盘字节不是网络传输字节，后者明确未知。

## 固定下载与编译配置

四个构建依赖的压缩下载共 199,412,970 字节；成功编译后才另取原 Wasmer archive 302,721,336 字节。所有下载都校验发布的**完整字节数与 SHA256**，校验成功后才解包或执行；没有 `latest`、在线依赖自动下载或全局安装。解包逐项使用 stdlib data filter，拒绝越界路径、特殊文件和超出记录的扩展上限。

| 依赖 | 固定发布 | 压缩字节 | SHA256 |
| --- | --- | ---: | --- |
| WASIX compiler wrapper | [wasixcc 0.4.7](https://github.com/wasix-org/wasixcc/releases/tag/v0.4.7) | 3,258,983 | `d39db81b6b76d9567e0f61955f390b8586d932ab683816989e23305f27099c2f` |
| 官方 WASIX LLVM | [21.1.206 Linux x86_64 GNU](https://github.com/wasix-org/llvm-project/releases/tag/21.1.206) | 75,596,113 | `ab32cf976226dc9478c9cb8331fa877163ff34d78b190fb201d6eac92cb7dab0` |
| 官方 WASIX libc／libc++ sysroot | [2026-10-02.1，非 EH](https://github.com/wasix-org/wasix-libc/releases/tag/v2026-10-02.1) | 5,951,821 | `ffc1a6f52e81a4945a9df7bb2fee632a051403eb64316b006543ea54cd779c6b` |
| Binaryen | [128 Linux x86_64](https://github.com/WebAssembly/binaryen/releases/tag/version_128) | 114,606,053 | `4ce79586d1c4762502eebe9a1db071fa5e446ef8897f2f766eb1cce5ec6dee9e` |
| 原 Wasmer runtime | [7.5.0](https://github.com/wasmerio/wasmer/releases/tag/v7.5.0) | 302,721,336 | `23f99818deaad93859f8bb8292af81f0c39f25aea19eaccfb72fc7c9e8a3fae9` |

构建脚本中的 source commit 进一步固定每个依赖的源码身份；这不等同于声称重新构建了这些官方工具。宿主 GCC、GNU Make 和 Python 来自实际 Ubuntu runner，记录执行路径、版本输出、字节数和 SHA256，保留它们随 runner image 变化的事实。

目标编译使用真实 `wasixcc`／`wasix++`，host 生成器使用 GCC/G++。显式非 EH sysroot 保持原 Cranelift runtime 路线；[wasixcc 的编译及链接实现](https://github.com/wasix-org/wasixcc/blob/d59389f68277c60b65cb1cc1524de641d845d336/src/compiler.rs) 设置 WASIX atomics、shared memory、libc／libc++ 与运行时入口；其非 EH 的 `wasm-opt --asyncify` 步骤仍启用。没有因构建大小而删去该运行语义。`make -j1`、linker `--threads=1`、`BINARYEN_CORES=1` 限制构建并发。

核心命令为：

```sh
python3 configure.py --dest-cpu=wasm32 --dest-os=wasi --cross-compiling --openssl-no-asm
make -C out BUILDTYPE=Release V=1 -j1 node
```

OpenSSL 保持启用，只有不适用于此 target 的 native assembly 关闭。其它源码功能遵循这份固定 upstream configure 的实际结果，记录 `config.gypi` 等原件，不对未知编译失败临时切换特性、替换 JS 引擎或复用预编译模块。

## 执行、资源与结果边界

[独立工作流](../../../.github/workflows/node-wasix-source-build.yml) 仅由 manual dispatch 或精确 PR 标签 `run-node-wasix-source` 的一次事件进入；没有 push／synchronize 重跑，没有取消正在清理的旧任务。本提交没有触发远端。它调用 [构建脚本](../../../scripts/build_node_wasix_journey.py)，复用 [原 runner](../../../scripts/real_node_wasix_journey.py) 的资源观察、owned process group 和原 cell 判据。

顺序是 fresh source fetch → 确认 commit/tree 与 `git fsck` → 下载校验固定工具链 → fresh configure/build → 确认 tracked source 未改变、新输出为 WASM → 保存模块与构建记录 → 原 runtime 旅程。`out/` 必须在源码 checkout 后不存在；新模块的 hash 不能等于失败 release 或源码树里旧 `node.wasm`。新模块原字节、完整编译输出、实际工具身份、配置、关键源码、所有资源／进程观察及最终失败均上传。

`build-attestation.json` 明确是 **本次运行的 unsigned build record**，不冒称签名供应链 attestation 或确定性可重现构建。只有源码与输出检查完成才写入 `build_succeeded=true`；它本身不表示 cell 成功。

每一步重读真实系统及可见 cgroup v2 全祖先约束。起始磁盘预算为 24 GiB；这是该诊断选择的有界预算，**不是已测得的最小编译需求**。构建保留 `max(2 GiB, 可见 headroom / 4)`，余量作为 sampled group RSS 预算，上限 10 GiB；不足 4 GiB 起始 RSS 预算时明确拒绝。磁盘 reserve 为 4–8 GiB。每个子进程有 24 GiB AS、2 GiB 单文件和 CPU 硬限，另有群组 RSS、可用余量、磁盘、128 MiB 输出的采样停止门。整个源码阶段累计最多 7,200 秒；这些时间和内存上限尚未证明足以构建该 V8 移植。工作流上限 145 分钟给后续原 guest 及证据上传留下空间。

只有固定公共 source fetch 子进程允许网络；编译、生成器和 guest 子进程继承清空后的具名环境，seccomp 拒绝网络。所有 subprocess 继续先持久记录 spawn 意图和 PID，保留首次停止原因，TERM/KILL 有界等待并确认 leader 与整个 process group；未知退出不会成为可用的稳定快照。原 guest 继续使用 12 GiB AS、实际 headroom 决定的最多 6 GiB sampled RSS、180 秒 wall、150 秒 CPU、禁 cache、仅 owned guest fs。进入 guest 前再次验证原 driver，要求 cell/state/witness 未被构建阶段写入。

原 `FIRST_SOURCE`／`SECOND_SOURCE`／`REOPEN_SOURCE`、framing、`run_journey` 与成功判据保持不变。仍须真实 Node/V8 身份、1517 fs witness、同进程 1518 lexical state、两份成功报告和 checkpoint，才可条件性 reopen 到 1519。遇到新的 runtime builtin 或构建错误，保留原件后定位真实缺口；没有将退出失败、缺 cell 或未执行的 reopen 判为通过。

## 本提交的验证

本机仅作 Python AST／语法、原 driver 与输入／判据不变性、workflow 解析、固定资产元数据核对和文档完整性检查，记录于 [静态检查](static-checks.json)。没有下载以上大资产，没有编译 Node/V8，没有执行新 guest，没有新增 scenario 矩阵。正式结果须绑定后续真实 hosted run；Bun 与 Shenko WASIX backend／安装／依赖 generation／恢复仍是独立待完成项。
