# 固定 Node-WASIX 源码构建：OpenSSL 归档前命令长度失败

[实际 run 43922989826](https://github.com/cyjin-yl/shenko/actions/runs/43922989826) 的唯一 job 于 **2026-11-16 18:47:00 UTC** 完成，结果为 **failure**。固定源码的配置和编译确实运行；`compile.stderr` 第 250666 行为 `make: printf: Argument list too long`，下一行在 `libopenssl.a` 归档目标处报告 recipe Error 127，最外层 make exit 2。原三段 cell、fs witness 和条件性 reopen 均未触达，没有安装或证明 Shenko 的 Node 能力。

## 原件和身份

| 项目 | 实际记录 |
| --- | --- |
| Workflow / job / attempt | `43922989826` / `133494902423` / `1` |
| PR head | `51a1ce435b3f83c52dcad11f19c522acb77cb59b` |
| 实际 checkout | `61a813576b19b3aea0403ef6dd4fc3d64238ad0b` |
| 实际 checkout tree | `f7c39079c0788091c311c3f01264fa2846e398f3` |
| Node 上游源码 | `Multi-V-VM/node-wasix32`，commit `20871d1fb49e1af91b1fc729b0cf4e119c0bfd6f` |
| Node 上游 tree | `1f91f029db924986b371c4b848fb65ffd080613d` |
| Artifact | `14258178649`，2,152,524 字节，98 个 entry |
| 原 ZIP SHA256 | `56ac75e7ca5580b4c0ce19c5a7dcc96f61ffb31e049f67f323194fb33fb8b73d` |

[原始完整 ZIP](original-artifact.zip) 保留全部 98 个文件，包含完整的 `compile.stdout`、`compile.stderr`、640 份资源样本、执行过的工具版本、下载校验、生成配置、固定源码关键文件、原 driver 与原 framed inputs。原 ZIP digest 与 GitHub artifact 元数据相符，逐 entry 的 CRC 与 SHA256 已独立核对；内部 `files.json` 的 97 项大小和 SHA256 全部匹配，第 98 项是清单本身。详见 [ZIP 校验与全部 entry 清单](zip-verification.json)。

[job 原日志](original-job.log)、[完整 jobs API](original-jobs-full-api.json)、[run API](original-run-api.json)、[artifact API](original-artifacts-api.json) 与 [实际 checkout commit API](original-checkout-commit-api.json) 保留来源身份。为便于审阅，部分原件另按原字节展开在 [artifact](artifact/)；没有展开到 Git 的两个大编译日志仍完整保留在 ZIP，公开的小 [错误定位索引](compile-error-excerpt.json) 明确标注源文件 hash 和行号。分析写入独立的 [observation.json](observation.json)，不重写原 summary。

## 实际阶段和资源

编译命令为 `make -C out BUILDTYPE=Release V=1 -j1 node`，17:53:38.562393 UTC 开始，18:46:54.067962 UTC 结束，经过 **3195.484 秒**。原观察为 `exit_code=2`、`stopped=null`、`leader_reaped=true`、`group_remaining=[]`，没有资源监视器或外部停止动作。未到最终 Node module link、后续 wasm-opt 或 guest 执行，不能把配置成功、部分 C/C++ 编译成功换成 Node 运行通过。

本轮采样 group RSS 峰值 **3,328,528,384 字节**，640 份样本中最小有效可见内存余量 **12,561,895,424 字节**，最小空闲磁盘 **89,381,711,872 字节**。编译预算为 10 GiB 采样 RSS、24 GiB 单进程虚拟地址、约 3.64 GiB 内存余量、8 GiB 空盘余量，剩余 compile wall budget 为 7165.673 秒。原件没有 OOM 或 guard 触发，这次失败不能归因为观测到的物理容量不足。可见 cgroup 祖先与系统内存的范围仍按原记录保留，不声称能看到 namespace 外部约束。完整记录见 [compile.observation.json](artifact/compile.observation.json) 和 [build-resource-budget.json](artifact/build-resource-budget.json)。

工具、driver、输入都保留原绑定：[manifest](artifact/manifest.json) 中 builder、runner、driver 和首轮输入 SHA256 逐项与归档 bytes 匹配。首轮原 framed input 仍为 980 字节，SHA256 `f1c895141dd1cbb589de36e6c93900da14b1f24f96ee24c2016afc303d43c0dc`。它只是准备好的输入，本轮没有交给 guest 执行。没有通过缩小输入或更换 framing 避开先前 `BigIntConstructor` 缺口；那个已运行 binary 的失败仍独立成立。

## 可定位的后续工作

实际生成的 [out/Makefile](artifact/build-configuration/out/Makefile) 第 214 行定义 `exact_echo`，将完整命令放入一个 `printf` 参数；`do_cmd` 第 261 行打印命令，第 268 行写入保存的命令。日志中非常长的绝对对象路径和 `V=1` 为命令长度问题提供了具体调查方向，但本轮没有保留 `openssl.target.mk`，也没有捕获被拒绝的 exec argv，尚不能证明具体哪一个 `printf` 参数或操作系统长度上限触发了错误。

下一步应根据固定 GYP 规则重现并修正这一个构建命令长度边界，同时增加该生成归档规则的原件保留。`V=0` 或短目录目前都只是待验证方案，不能称已经修复。没有重复触发本轮构建，没有改原 driver、协议、cell 判据或 Shenko availability；独立 Bun 目标以及 Node 的正式安装、配额、恢复和完整验收仍保留。
