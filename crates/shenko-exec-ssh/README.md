# shenko-exec-ssh

宿主执行 provider：通过本机已安装的 **OpenSSH `ssh` 进程**执行远端 shell 请求，并提供 tmux seat 的兼容路径。没有使用 russh；这不是 SPEC R019/A09 的真实 WASM SSH 实现。

## 为什么采用这条路径

原远端分支只返回“需要 russh”，并没有传输。宿主 OpenSSH 能在不改 workspace 依赖的前提下实际连接、认证并校验主机身份，补齐 R018 的一个执行路径；本轮另提供受持久任务记录约束的原生 SSH runner；WASM SSH、完整文件/seat 工具与完整任务服务仍保留为未完成义务。

## 最小使用方式

```rust,no_run
use shenko_exec_ssh::{SshExecutor, SshTarget};

# async fn example() -> anyhow::Result<()> {
let target = SshTarget {
    host: "verified-node.example".into(),
    user: "worker".into(),
    port: 22,
    key_path: Some("/run/shenko/credentials/worker-key".into()),
    known_hosts_path: Some("/run/shenko/credentials/known_hosts".into()),
    ..SshTarget::default()
};
let result = SshExecutor::over_ssh(target)
    .run_with_timeout("printf '%s\\n' ready", 30).await?;
// 保存完整 result；unknown 时查证原任务现场，不自动再次执行。
assert!(result.success());
# Ok(()) }
```

配置里的示例路径不是实际凭据。需要 OpenSSH 已支持的目标 shell；命令内容是**远端 shell 程序**，不是跨平台 argv 化 exec。

- `SshExecutor::new` 保留旧便捷行为：`localhost` / `127.0.0.1` 直接执行本机 `sh -c`，忽略 SSH 用户、端口与 key，不能据此宣称 SSH 认证成功。
- `SshExecutor::over_ssh` 始终走真正 SSH，包括回环地址。`TmuxSeat::remote` 使用此方法；`TmuxSeat::local` 仍走本机便捷路径。
- 远端必须显式给 `key_path` 或设置 `use_agent=true`。后者使用当前 `SSH_AUTH_SOCK` 的已授权 agent；不启用时禁止 agent，绝不静默使用默认密钥。密钥文件由调用方保护和清理，执行器不复制或读取其明文。
- 旧 `password` 字段仅能被反序列化：Debug 脱敏、序列化跳过；只要设置，执行前就报不支持。没有密码 argv、密码日志或虚假密码认证。
- 使用 `-F none` 忽略用户和系统 ssh_config，关闭本地命令、隐式代理、连接复用、转发与交互密码提示。这里尚不支持 ProxyJump/ProxyCommand 或用户 SSH alias。
- 始终 `StrictHostKeyChecking=yes`、`UpdateHostKeys=no`、`BatchMode=yes`，未知或变化的 host key 会被拒绝，没有自动 TOFU。未指定 `known_hosts_path` 时仅使用 OpenSSH 默认信任文件；指定时只使用该文件，且不写入它。显式路径需是 UTF-8 绝对路径，不含换行、引号、反斜杠、`%` 或 `$` 展开符。
- host/user 采用保守字符集，不接受以 `-` 开头的值；端口不得为零。命令、目标、端口、用户、key 均通过独立本机 argv 传递，选项以 `--` 终止。命令不会经本机 shell 再解析。

## 结果、超时与取消边界

`ExecResult` 保留原字段，并新增可选 `execution_id`、`outcome`、`diagnostic`。serde 缺省兼容旧结果；旧数据的 outcome 不会被伪造成新观察。新增结构字段会影响外部 Rust struct literal；本仓库未发现 crate 外构造点。

| 观察 | 结果 |
| --- | --- |
| SSH 正常报告 0–254 的退出码 | `outcome=exited`，非零就是远端非零退出；仍可能有部分外部效果 |
| SSH 返回 255 | `outcome=unknown` / `reason=ssh_exit255`；传输失败和远端 `exit 255` 有歧义，不能冒称已区分 |
| SSH 进程被信号终止或无法获得完整输出 | `unknown`，保留已收集 stdout/stderr 和诊断 |
| 远端调用超时 | `timed_out=true`、`outcome=unknown`；停止并 wait 本机 ssh，不代表远端程序已经停止 |
| 本机便捷路径超时 | 显式 kill / wait 直接子进程；成功回收后 `outcome=timed_out`，失败则 unknown |
| 参数校验或 spawn 失败 | 返回 Err；没有成功派发本次进程 |

`success()` 只接受退出 0、未超时且非 unknown 的结果。tmux 读取/新窗口路径不会将 SSH unknown 吞成空结果或成功；读取错误可通过 `anyhow::Error::downcast_ref::<ExecFailure>()` 取得完整结果，写入路径直接返回完整结果让调用方持久化。tmux 输入不是应用接收或任务完成的证明。

Unix 为每次调用创建独立进程组，超时或 future drop 时向该组发送 SIGKILL；直接子进程使用 `kill_on_drop(true)`，超时路径实际 `wait`。future 被取消后由 Tokio 尽力回收，不能把 drop 冒称持久取消已完成；runtime 已终止时也不承诺执行异步清理。非 Unix 仅有直接子进程终止/回收，不承诺进程树终止。逃逸进程、先前文件/网络效果、远端进程不受本地组的停止证明覆盖。

`execution_id` 是本次本地调用的关联 ID，**不是远端 operation handle，不保证幂等，也不内建持久查询服务**。整合者须在外层 §41 的 T1 中先持久保存 operation/attempt 和目标，再调用执行器；T2 保存完整结果，按 operation/attempt 关联此 ID。unknown 进入 reconciliation，按原 seat/任务、进程或可验证后置条件查询现场；没有明确证据前不能自动重发。调用 future 被 drop 时没有返回结果，外层持久 supervisor 须依据已 armed attempt 保持 unknown，而不是记录取消成功。

## tmux seat 发现与受守卫的写入（C07 / #12 切片）

- `TmuxPane` 现在携带 `pane_pid` 与 `dead`（`pane_dead`）：seat 身份是 pane + 进程现场，不是永久不变的 pane 编号。
- `discover_agent_seats` 返回 `AgentSeat { pane, kind, processes }`：对每条 pane 从 `pane_pid` 向下收集真实进程子树（`ps -axo pid=,ppid=,args=`），跳过 shell/tmux/sudo 等 wrapper 后按第一个非 wrapper 程序归类 `claude`/`omp`/`codex` → 对应 `SeatKind`，其余 → `Other(comm)`；裸 shell 和 dead pane 不再被当成 seat。`ps` 不可得时退化到 `pane_current_command` 分类且 `processes` 为空——调用方应把该 seat 视为未核验。
- `send_keys_verified`/`send_input_verified` 在同一条远端命令里先检查 pane 存在、非 `pane_dead`、`pane_current_command` 仍等于 `expected_command`，再 `send-keys`；三种拒绝映射为可下转的 `SeatRefusal`（`PaneMissing`/`PaneDead`/`ProgramChanged`），不再把为 seat 准备的输入盲发进 shell。检查与发送之间仍存在 tmux 无法消除的竞态窗口——这是拒绝盲输，不是送达证明。
- `SeatObservation.screen_hash` 是纯文本画面的稳定 digest，供调用方在回答前比对问题/画面 revision 是否变化。

## 本轮验证范围

仅新增两个针对性测试，串行运行：

```sh
CARGO_BUILD_JOBS=1 cargo test -p shenko-exec-ssh -- --test-threads=1
```

1. 临时脚本模拟 SSH：检查 argv 边界、严格 host 校验/隔离配置参数、凭据脱敏、255 unknown、部分输出和旧 serde 结果兼容；脚本不会登录远端。
2. Linux 本机超时：输出真实子进程 PID，确认返回时 `/proc/PID` 已消失，证明已 wait 回收，而不是只设置超时标志。

模拟测试不证明真实 SSH 认证或 WASM 兼容。真实回环 sshd 验证若执行，使用仓库外临时生成的 client/server keys、独立 known_hosts 与仅监听回环的隔离服务；不得读取用户实际私钥或修改用户信任文件。

## 仍未交付的范围

- R019/A09：WASM SSH/keygen/transfer、托管签名桥；本 provider 不能抵扣这些目标。
- R016/R017：密钥创建、轮换/撤销与公钥 UI。
- R041：stat/read/write/hash patch、SFTP、stdin、stream/poll/cancel API、输出截断及有界 spool；本轮只提供有界 stdin/输出、1MiB upload/download；完整文件 API、流式 spool 与远端取消仍未交付。
- §17.1/§17.5/§35.5：持久 seat/lease 记录、版本化问题协议、高亮/grid 状态机、真实 Claude/OMP journey 仍未交付。本轮补上了进程子树 seat 识别与受守卫写入，但既有 tmux 观察仍只是兼容路径，不能宣称完整通过。
- §36/§41、RT09/P41-03：本轮有 task-bound T1/E1 和远端 receipt 的原生适配；完整调度器 T2、祖先取消、授权 registry、writer/node fencing、残余 supervisor 与真实部署验收仍未接全。不保证 arbitrary shell 的业务效果已完成或可撤销。

这些是已有 SPEC 范围，不因本轮实现或测试通过而标记 satisfied。

## 受持久任务约束的原生 SSH 切片（C07 / #12）

`SshTaskRunner` 是可信 child/execution-service 侧 API，不直接接受聊天正文声明的权限。
它复用 `shenko-store` 的 Task/Operation/Attempt/Evidence 表和 CAS，不另造一个内存任务表。

- `SshBinding::register` 要求显式 known_hosts 与 key/agent 来源，将**公有**信任材料封存到 CAS；不复制私钥。每次真正 `over_ssh`，不使用 localhost host 直通。
- dispatch/query 在当前 CAS 核验所封存的信任对象，再解析为 OpenSSH 所需的绝对路径；Store 以相对 CAS 根打开时也可使用，不回退到历史 locator 或系统 host-key 库。此修正不声明已解决现有 CAS 路径验证到 OpenSSH 打开之间的 ancestor-swap TOCTOU。
- `execute` 在一笔 IMMEDIATE SQLite 事务内查 `(task_id, logical_key)`、校验当前 task/record revision、running invocation、epoch、当前任务取消及 admission 状态，再固定输入、目标/合同、operation/armed attempt 和 task memo。事务外才 SSH；同 key 不同参数/绑定/合同冲突，同 key 命中只查询原 operation，旧 context 也不会再派发。
- `SshAction::Exec` 以 argv + 二进制 stdin 执行，默认不经过远端 shell。显式 `sh -c` 仍是任意 shell，调用者承担它的真实效果。`Upload`/`Download` 各是一项独立 operation，限 1MiB。上传写同目录独立临时文件、fsync、rename、fsync 目录；不会声称跨本地/远端原子。
- 对端要求已登记的 POSIX/Python3 能力及私有、账号所有的 receipt root。v1 helper 用 UUID 目录原子认领，先持久 intent，再执行；receipt fsync/rename 后才返回。目录已存在但 receipt 缺失时保持 unknown，绝不重新运行。helper/信任摘要和 receipt reader 版本固定在记录里。
- 真实 SSH 超时/255/断线后保留 armed/unknown。`query(operation_id)` 只核验原路径、输入 seal 与原始 receipt，查询缺失不是重发许可。query 的 SSH 进程退出 0 也不是命令完成；须有匹配的 completed receipt。
- E1 将原始观察 seal 到 CAS，并原子追加证据与 operation/attempt 投影。迟到证据可以在取消后入账，但不推进旧 task。SQLite settlement/CAS 错误会关闭此 Store 的原生 external writer；重开后先查询原 identity，不在旧 session 重试效果。
- **进程结束不等于业务完成。** exec receipt 可确认退出码（包括真正的远端 exit 255），但 arbitrary exec 的 effect_status 仍 unknown、operation 保持 reconciling；attempt 可 ended。成功文件上传的声明域是写入给定 bytes，下载是 read-only。runner 从不将 TaskRecord 置 terminal；完整业务 acceptance/T2、取消 supervisor 与 C17 节点归属/fencing 由后续服务接续。
- 传输每路 stdout/stderr 最多保留 2MiB 并继续排空，超限返回 output-unavailable/unknown，避免无限内存队列。helper receipt 单路输出有界，明确带 truncated；对端原始输出文件仍需受管磁盘配额。future drop 不是 durable cancel，不能证明远端停止。

可运行的可信 child 驱动为 `examples/task_ssh.rs`：对现有任务使用 `execute`/`query`/`task`；`fixture-init` **仅**创建空账本内的合成任务供隔离实验，不是生产身份/授权入口。

```sh
CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0 cargo build -p shenko-exec-ssh --example task_ssh
# task_ssh execute /private/config.json
# task_ssh query /private/config.json
```

配置只放在本机受保护文件，包含 db/cas、可信 TaskInvocation、已登记 target、operation_id、logical_key、action 与 timeout；不要提交真实密钥、账号 endpoint 或对话。该驱动证明可运行的原生 adapter，尚未把主体的 durable delegation outbox、完整 registry 授权和生产 child scheduler 接全，也不替代 C01/seat/WASM/多节点验收。
