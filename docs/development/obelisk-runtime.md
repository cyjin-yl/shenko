# 可选 Obelisk 进程桥

实现位置：`crates/shenko-exec-host/src/obelisk_runtime.rs`。桥本身不注册插件、不拥有安装事实，
不替代主体原始 session、MemoryBackend 或节点同步。安装、原生任务回执及完整文件归档由调用方组合。

## 实际绑定的上游

本次兼容绑定是独立项目 [tommy0103/obelisk](https://github.com/tommy0103/obelisk)
的 `cc75c2dd1aead4f4374758a7536385307934e177` 源码 CLI，许可证为 AGPL-3.0-only。
没有复制其实现进 Shenko，也没有将其纳入 Shenko 自有代码的双许可承诺。

**不能凭 `--version` 判定兼容。** 实际检查发现，npm 发布的 `@obelisk-apps/cli@0.3.0`
与上述 main 的行为不同：发布包未支持 `OBELISK_HOME`，`index_state` 也没有当前
`replay_required` 字段。先运行普通查询来探测会已经刷新默认历史，所以桥在任何上游启动前，
按 `obelisk_upstream_manifest.json` 核对 51 个源码、schema 与 package 文件的长度及 blake3。
配置的 argv 必须选择已核验 checkout 的 `packages/cli/src/obelisk.ts`；Node 可执行文件由配置明确指定。
更新绑定必须重新检查实际源根、独立 index home、查询形状和原件读取，再更新该清单。

这证明读取到的安装字节匹配已审计组件，不是对恶意并发改写本机软件的隔离承诺。
源码 checkout 由安装者持有；桥不自动下载、安装、修补或改变第三方代码。

配置示意（路径均是部署者实际选择的本机路径）：

```json
{
  "executable": "/absolute/path/to/node",
  "source_checkout": "/absolute/path/to/obelisk",
  "arguments": ["/absolute/path/to/obelisk/packages/cli/src/obelisk.ts"],
  "index_home": "/absolute/path/to/disposable-obelisk-index",
  "source_instance": "coding-history-origin",
  "sources": [
    {"provider": "codex", "root_label": "codex", "path": "/absolute/path/to/.codex"},
    {"provider": "omp", "root_label": "omp", "path": "/absolute/path/to/.omp/agent/sessions"},
    {"provider": "pi", "root_label": "pi", "path": "/absolute/path/to/.pi/agent/sessions"}
  ],
  "limits": {"timeout_ms": 30000, "stdout_bytes": 1048576, "stderr_bytes": 65536, "drain_timeout_ms": 2000}
}
```

Node 24 可直接执行这一份 TypeScript 源码。`index_home` 是可重建缓存；原件目录保持只读使用。
桥原子创建完整 settings 文件，并拒绝覆盖已有不同设置。所有已知上游 provider 均显式设置，
没有选中的 provider 不参与默认历史发现。根标签是逻辑名，绝对路径只属于当前节点。

## 查询与原件是两层

`ObeliskQuery` 支持 `sessions`、`search`、`messages`、`raw_page`、`manifest` 和显式 `script`。
JSON 参数通过序列化放进 JS，进程通过 `Command` argv 启动，不经过 shell。
自定义脚本可以组合上游 helper 和只读 `sql()`；这个 API 合同不意味着任意 JavaScript 都只读。
上游使用 `node:vm` 方便组织作用域，并未提供安全隔离。观察明确记录自定义脚本拥有本机执行权限。

`search` 可以在 FTS 无命中后对**同一索引投影**进行有界中文子串回退，保留原 scope。
它不会把被上游截掉的原文尾部变回来。返回的 `full_original_coverage` 保持 false；完整原件
应另行封入 Shenko CAS，再从完整资料建立可重建索引。

`raw_page` 以 UTF-16 code unit 为上游偏移单位，避免在 surrogate pair 中间切出无效 JSON；
返回下一偏移及实际长度，缺失／变化／不可读取的源返回 `source_unavailable`。连续页不构成
跨 refresh 的快照。需要稳定原文版本的调用方先归档完整源文件，使用该内容 hash 读回。

`manifest` 从实际 `index_state` 枚举源文件，包含没有独立 session 行的 Codex 子代理文件。
只有一个 session 明确对应该路径时使用其 native ID，否则用配置根标签与相对路径作为逻辑原件名，
并披露 `identity_basis`。它不从路径猜测角色或捏造原始字节 offset。
消息记录只带 native ID、role、时间、parent、agent 与 visibility 元数据，不把 10K 截断投影称作原文。
上游 cursor 可以含超过 100 KiB 的 Bloom filter，因此只披露大小，完整 cursor 留在本地派生 DB。

`source_instance`、`source_key`、原件内容版本分别表达来源身份、逻辑原件与这次实际字节。
同一原件跨机器复制不按导入节点或绝对路径产生新身份；路径派生逻辑名发生 rename 时需要新的观察或显式映射。
索引时间、文件 mtime 与消息墙钟时间都不证明分布式顺序、全量完整性或独立证据数量。

## 生命周期与可观察限制

stdout/stderr 同时读取并限制内存；达到输出预算、取消或 host wall-clock deadline 后请求停止进程，
等待原进程回收，继续有界 drain，然后 join 原来的 reader。读回、回收与终止请求各自有字段，
不会把超时或 `kill_on_drop` 当成已停止的证明。

Linux、Android、Apple 平台对进程组发停止信号；其他平台只声称直接 child，未证明逃离进程组的后代停止。
主调用方需要 await 取消后的收尾，不能丢弃 future 就宣布 quiesced。资源超限保留失败状态，
不接受截断 JSON 为成功。query 前的索引刷新可能有延迟／警告；coverage 明确保留 incomplete/unknown，
不会把无命中解释成源历史不存在。

## 最小真实验证

两个测试位于 `obelisk_runtime/tests.rs`，未配置真实 CLI 时明确输出未执行：

```sh
SHENKO_OBELISK_EXECUTABLE=/absolute/path/to/node \
SHENKO_OBELISK_SOURCE_CHECKOUT=/absolute/path/to/audited/obelisk \
cargo test -p shenko-exec-host --lib obelisk_runtime::tests -- --nocapture
```

第一条真实流程覆盖三种 source、中文回退、完整 raw 分页与源消失；第二条先证明真实 CLI 可运行，
再进入异步计时器，验证 host deadline 收尾。它们不证明本地模型会自行采用检索、网络分区同步，
也不证明全部上游 Harness、Windows 进程树或长期运行。那些证据由相应调用方单独记录。
