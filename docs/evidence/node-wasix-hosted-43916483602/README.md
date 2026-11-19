# 首次 hosted Node-WASIX：取得真实 runtime 缺口

[实际 run 43916483602](https://github.com/cyjin-yl/shenko/actions/runs/43916483602) 于 2026-11-16 完成，结果为 **failure**。固定 Node-WASIX module 的 stderr 报告 `WASM32_UNSUPPORTED_BUILTIN id=256 name=BigIntConstructor`、`TypeError: BigIntConstructor` 与 `Node.js v25.0.0-pre`。没有成功 cell 报告、fs witness 或 checkpoint，条件性 reopen 没有尝试。这个实际内建函数缺口不会因增加资源预算得到修复。

## 原件与身份

| 观察项 | 实际身份 |
| --- | --- |
| Workflow/job | `43916483602` / `133471893851`，attempt 1 |
| PR head | `be46be34593452f47b824d3ae62f625032e2d54c` |
| 实际 checkout | `5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c` |
| 共同 source tree | `58e5e59b230a5020ee56ccf800deea1466c3151b` |
| 原 artifact | `14249304171`，38,884 字节，26 个文件 |
| 原 ZIP SHA256 | `2d0fb9a4f51a23c506c360ebfadb6aa42e2f3eeafd75ca9e9d03896120657106` |

[完整 ZIP](artifact.zip)、[全部展开原件](artifact/)、[job 原日志](job.log)、GitHub 原元数据与 [ZIP entry 清单](archive-entries.json) 均保留。ZIP hash 与 GitHub 的发布 digest 相符；artifact 内部 `files.json` 的全部 25 个记录也逐一校验，额外第 26 个文件是该清单本身。实际 checkout 的 Git tree 已独立核对。后续分析汇入 [observation.json](observation.json)，没有重写原件。

运行中的 runner SHA256 为 `6218ed78275c4423160c93fbdb384901a7fd89b2a8eaf06f01efcad464abebaa`，driver SHA256 为 `a9419c9aa0abbced7c69145806768153971d37121a8c20e63e0c623f0991f150`，实际首轮 framed input 为原 980 字节，SHA256 `f1c895141dd1cbb589de36e6c93900da14b1f24f96ee24c2016afc303d43c0dc`。三者与发布候选及前两次本机尝试的对应输入一致。

两个实际下载均通过固定大小和 SHA256 校验，Wasmer executable 的独立 hash 也一致。运行时实测 `wasmer 7.5.0`。原 manifest 保留具体 URLs、资源及 driver 绑定；Node release asset 的源码构建 attestation 仍缺失，不能把 tag/main 中的源码直接说成这份 binary 的可复现构建来源。

## 真实阶段与资源

首个 guest 于 17:15:53.120846 UTC 启动，58.38 秒后普通 exit 1。没有触发 wall、CPU、RSS、磁盘或外部停止；`stopped` 为 null，cleanup attempts 为空，leader 已回收且 owned group 为空，cache 无文件。实际命令运行的是固定 `node.wasm`，只映射 owned `/workspace`，保留 clean env、单线程编译、禁 cache、offline 及 seccomp 网络拒绝。

本轮资源观察足以将失败与先前人为 1.5 GiB RSS 门区分：预算为 6 GiB RSS、12 GiB 虚拟地址与约 3.67 GiB 余量，实际 RSS 观测峰值约 **1.705 GiB**。59 个记录样本中最小有效内存余量约 **13.103 GiB**，两个可见非根 cgroup 的 `memory.max/high` 为 `max`，观测 `oom/oom_kill` 为零；CPU affinity 为 4 个逻辑 CPU。完整原始数值与 cgroup 链见 [first.observation.json](artifact/first.observation.json) 和 [resource-budget.json](artifact/resource-budget.json)。这些是可见环境的观测，不声称测到了 namespace 外部约束。

当前 guest 目录仅有原 driver 与环境 package.json，没有 cell out/err/report、state.json 或 witness.json。stderr 的版本文本是实际 guest 输出，但尚没有 cell 内 `process.versions` 观测。失败阶段因此记为原 driver 启动/协议入口未完成；没有 stack trace，不能把推测的第一处 throw 位置当成动态证据。

## 源码解释与后续修复边界

原 [Shenko driver](https://github.com/cyjin-yl/shenko/blob/5e7a3d5ade83d21fbc6e86b7eb8c3f19fb3fbe3c/crates/shenko-repl/src/drivers/js_cell_driver.mjs) 的两个 u64be 长度读取调用 `Buffer.readBigUInt64BE`。固定 release tag `0d66fea3` 的 [internal/buffer.js](https://github.com/Multi-V-VM/node-wasix32/blob/0d66fea37737024056983f35e84af3e8448ece08/lib/internal/buffer.js) 在该实现中将两个 32 位半值转为 BigInt。因此，原协议路径确实依赖这个 runtime 内建函数；这一静态依赖仍不能替代缺失的动态 stack。

已经按 Git blob SHA 独立验真的两个源码文件显示了一个具体修复方向：release tag 的 [wasm32 builtin fallback](https://github.com/Multi-V-VM/node-wasix32/blob/0d66fea37737024056983f35e84af3e8448ece08/deps/v8/src/builtins/wasm32/builtins-wasm32.cc) 没有 `BigIntConstructor` 分支；固定 main `20871d1f` 的 [同一文件](https://github.com/Multi-V-VM/node-wasix32/blob/20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f/deps/v8/src/builtins/wasm32/builtins-wasm32.cc) 从第 18,157 行起明确处理该 builtin，调用 `BigInt::FromNumber` / `BigInt::FromObject` 并保留异常结果。完整来源/hash 与观察范围见 [source-observations.json](source-observations.json)。

这不意味着仓库现成 binary 已修复：固定 main 跟踪的约 81 MB `node.wasm` 最后一次变更仍为 2026-08-23 的 `11b23060`。它没有与上述后续实现绑定的构建 attestation，本轮没有下载或运行它。已运行的约 112 MB release asset 又不同于 tag 中约 100 MB 的 tracked module，三种身份必须保留。

下一可执行工作是核实固定 main 的真实 WASIX 构建入口、工具链来源、版本和所需资源，形成可审的 source-bound hosted 构建，再使用完全相同的原 driver 与 framed inputs 观察结果。本轮没有修改 framing 来绕过缺失函数，没有缩输入、提高期限或重复调度。新的源码构建路线尚未运行；即使以后该有限旅程成功，Shenko WASIX backend、安装/包 generation、完整恢复与独立 Bun 目标仍须各自实现和验收。
