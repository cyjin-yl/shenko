# 41 首版规范执行合同

## 41.1 本章的地位、范围与阅读方法

本章响应 U40，定义 Shenko 首版开发者发行可以据以实现、互操作和检验的执行合同。§1–24 说明产品目标与能力，§35–38 说明首版旅程及插件职责，§39 记录来源与差异，§40 规定开发方式；本章给出记录、状态、事务、接口、错误和交错结果。遇到前文“至少包含”“原子提交或同等恢复”等概述，以本章相应具体合同为准。用户已确认的产品要求优先，不能借细化改变永续主体、自主性、role 边界或插件化。

本章中的“必须／不得”是实现不变量；“首版默认”是开发 Agent 可自行实施和按证据调整的工程选择；“后端条件”必须来自真实能力协商，不能由接口外形推定。所有新增类型名和命令签名是 Shenko 拟定的协议，不是已发布 SDK、colibri-rs 现成 API 或已实现产品。Rust、WIT、进程内调用和网络传输可采用不同编码，但必须映射同一语义，并给出协议版本及 conformance 证据。

首次实现按以下顺序读取：共用提交与记录 → 主体及 inbox → operation/取消/回执 → broker/后端 → hook/插件调用 → seat/outbox → 升级与恢复 → 故障矩阵及完整轨迹。某个插件只需实现其适用合同；“一切插件化”不是让每个插件自行解释什么叫成功、取消或恢复。

**复用与扩展边界。** 本章借鉴固定版本 [Pi Durable SPEC](https://github.com/earendil-works/pi/blob/c60de42e98428dc1ef7440d78c23f5915966cdb3/packages/durable/docs/spec.md) 的单一 mutation line、已提交可见、外部 effect 与事务分离、持久 task/cancel、ownership 和重开恢复，尤其 §4、§5.1–5.5、§6、§7、§10.1、§11。Shenko 的单线程永续主体、物理 Engine broker、可替换全部实现、A/B 自迁移、多节点 availability branch 和跨代证据接收是基于用户要求的扩展。不得把 Pi 不提供的 CRDT/offline multi-writer、非合作代码强制终止或完整进程升级归为它已经解决。来源：REF-DURABLE、REF-USER、REF-DESIGN；行为 B03/B04/B20/B24/B36–B41。

### 41.1.1 定义写完的判据

每个有状态命令必须同时给出：调用方与状态所有者、必填输入、版本/幂等/前置条件、一次原子提交包含的记录、返回的持久回执、取消和迟到结果、重开后的处理、可观察事件及至少一个失败交错。缺一项即标记该合同尚待细化，不能以“provider 自行实现”掩盖跨模块歧义。与目标环境有关的性能值需要测量；未知值不能写成保证。

首版自动验收应覆盖下面的规范行为，而不要求一次实现所有未来设备和媒体 profile。后期 WASM 移植、通话、Computer Use、REPL、存储插件继续继承这里的 intent/effect/receipt、任务、资源和版本规则；它们各自还需满足原章节的能力验收。发布清单同时列出已交付和仍待交付的 R 项，不能用首版范围删除未来正式要求。

## 41.2 默认持久存储与观察边界

存储、任务调度、broker、生命周期和恢复器都由可替换服务实现；本节没有要求将 SQLite、全部业务表或升级策略永久编入 Rust 内核。默认发行装配一组实现相同合同的插件，小核心负责启动和连接当前有效服务。

### 41.2.1 本地写入域与 SQLite profile

首版每个本地 ledger 只有一条 mutation line、一个当前 writer incarnation。数据库写锁只覆盖本地校验与提交，不覆盖模型推理、SSH、网络、构建、插件等待或用户等待。只读快照不持有此写入线。独立节点拥有独立 ledger；通过事实导入、选举和 fencing 协议汇合，不通过同时挂载同一 SQLite 文件实现跨机协作。

默认 StoreProvider 使用本地 SQLite，配置并读回确认 `journal_mode=WAL`、`synchronous=FULL`、`foreign_keys=ON`；一笔逻辑 commit 对应同一个数据库中的一笔 SQL 事务。不使用跨 ATTACH 数据库写入来冒充一个全有或全无提交。FTS、向量、缩略图、检索排序和 UI 视图是派生物；原始 entries/receipts 与对应索引工作通知同笔登记，索引可稍后重建，不能因搜索暂缺就宣称原文不存在。索引返回 `indexed_through` 与 `source_through`；追求最新事实的读取直接查原记录或等待明确水位。

打包的 SQLite 版本必须包含官方 WAL-reset 修复（官方列出 3.51.3 及以后，或相应已修复的回移版本）；build manifest 记录实际版本与 compile options。是否支持 FTS5 在启动能力探测中明确记录。WAL 不用于多个主机共享的网络文件系统。以上 SQLite 特性与限制依据 [synchronous 文档](https://www.sqlite.org/pragma.html#pragma_synchronous)、[WAL 文档](https://www.sqlite.org/wal.html)；这是 Shenko 的首版配置选择，不等于 Pi 默认配置。

每个 `CommitReceipt` 返回 `commit_ref`、`durability_profile`、`store_incarnation` 和本次写入的实体 revision。默认 profile 为 `power_loss_local`，含义是在目标 OS、VFS、设备正确兑现同步语义的条件下，确认后应经受本机进程崩溃和掉电；它不包括磁盘物理损坏、失去整机、未同步远端副本或外部系统操作。某个平台只能证实 `process_crash` 时必须返回该等级，不能沿用名称继续宣称掉电持久。Agent 可自主选择调整 profile，但调整形成版本化配置事实，不回写已发出的 receipt。

### 41.2.2 对象与元数据的提交顺序

大文件、音频和截图放 CAS，ledger 只引用已封存对象。先完成对象写入、长度和 digest 校验，再按当前 durability profile 同步文件与必要目录、获得 `sealed_object_ref`，最后允许元数据事务引用它。事务回滚可留下无引用的已封存对象，由后续 GC 回收；不得先提交一个“对象存在”的引用再异步创建内容。

一个 checkpoint 的树根、文件 manifest 和所有新对象必须满足闭包验证；未知或缺少的对象导致 `ObjectClosureIncomplete`，不得将其提升为可恢复快照。流媒体尚未完成时使用显式 open-stream 记录，已封存分块与最终对象分开；下载 URL 不是已归档附件。宿主 RW 挂载和远端机器不在 CAS 快照的隐含范围内，仍按 operation 的实际 effect boundary 处理。

GC 读取一个已提交 root snapshot，pin 活跃/候选/恢复 generation、在途 attempt、未消费回执、迁移水位和用户/主体保留的档案；删除前重新校验 root revision 或使用等价 epoch 协议。不能因“旧软件已停”就删除其 receipt decoder、未完成恢复所需闭包或新版本运行后的事实。GC 自身有任务和删除回执，空间不足不会把普通写入悄悄降成不持久。

### 41.2.3 提交失败、发布与重开

调用通过 schema、幂等和 revision 检查之前的拒绝必须证明没有持久 effect，返回 typed rejection。进入 StoreProvider.commit 后，调用者取消只停止其等待，不撤销已接纳的存储结算。已开始的 commit 必须结算；只有耐久结果明确后，才采用内存状态并发布与该 commit 对应的事件。

StoreProvider.commit 抛错、返回不确定、存储已提交但内存 adopt 失败时，当前 ledger handle 进入 `failed`，拒绝后续 mutation，终止相应 watch，通知在途 invocation 停止新决定。不得在旧内存对象上重试“同一段代码”。恢复器重开 store、读取最后真实提交，再使用原 request/operation id 查询结果；原命令不存在且其幂等合同允许时才重新接纳。外部工作可能仍在运行，重开不能据此宣告它失败或释放其 Engine 占用。

状态观察遵循 `committed-only`：watch 返回不可变对象和 `commit_ref`；用户回调在写入线之外运行。首个快照含水位，后续事件按同一 ledger commit 顺序；断线重连使用该水位补读，超出保留范围返回 `ResnapshotRequired`。观察者报错或缓慢不回滚已提交事实，也不阻塞模型和任务写入。派生 UI 可以显示“生成中”，但不能把未提交 token/tool-call 当完成决定。

流式文本按有限 chunk 批量持久化后再作为可恢复内容发布，默认按 100 ms 或 4 KiB 中先满足者提交，终态强制 flush；该批量阈值是可调工程默认。未完成或被取消的 chunk 仍可作为带状态的原始轨迹保存，不成为自动重放的工具调用。只有通过完整 schema 校验并在相应事务中接纳的 action 才能执行。UI 的临时光标、传输速率等瞬时呈现不用写入每笔 ledger，也不得影响恢复。

本节对提交未知的处理参考 REF-DURABLE §10.1；持久等级参考 REF-STORAGE。特别不能把 Pi JSONL 的 `fsync:true` 误写成“每次已 ACK 尾部均保证掉电后存在”：其固定规范明确普通提交没有显式同步 main marker。Shenko 的不同 StoreProvider 必须各自通过其声称的恢复等级，不能只通过相同接口名称继承保证。

