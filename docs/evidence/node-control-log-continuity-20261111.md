# 控制日志 append 连续前缀与同 index/term 内容一致性 — issue #275

Date: 2026-11-11. Branch: `fix/node-control-log-continuity-20261111` (base `origin/main` `375238a`).

## 静态证据（修复前 `origin/main`，`crates/shenko-node/src/election.rs`）

- `pub fn append_control_entries`（`:1406`）：leader→follower 复制只验证
  `request.prev_index > 0` 时前任条目存在且 `previous.term == request.prev_term`
  （`:1416`, `:1426`）。
- 处理输入条目时**不要求**第一项 `index == prev_index + 1`，也不要求后续逐项
  +1：循环里只比较 `entry.term` 与 `control_entry(entry.index)`。
- 对已存在的同 index 条目只比较 term：`if existing.term != entry.term`
  （`:1449`）→ `log_mismatch`；term 相同就 `continue`（`:1453`）——**不比较
  kind/payload/完整 entry**，所以"同 index、同 term、不同 decision"会被静默
  当作已有记录，且 reason 保持 `"accepted"`。
- 任何新条目被接受即 `INSERT INTO node_control_log` 并 `apply_control_entry`，
  然后 `commit_index = commit_index.max(entry.index)`（`:1466`）——跨洞推进。
  部分应用：循环遇错 `break` 时，之前已应用的条目仍然生效。

## 修法（同一文件 + `crates/shenko-node/src/lib.rs`）

`append_control_entries` 重写为"全有或全无"：先预检整个请求，任何一项不通过
都返回 `AppendOutcome{accepted:0, commit_index:<原值>, reason:<显式原因>}`，
不 apply、不推进 commit_index、不推进 primary/config。

1. **当前持久前缀 / 请求 predecessor / 输入后缀的一致关系**
   - `prev_index > 0`：前任条目必须存在、term 必须匹配，且 `prev_index <=
     commit_index`（`"log_mismatch: predecessor entry is missing" /
     "predecessor term differs" / "the predecessor is beyond the committed
     prefix"`）。
   - 输入后缀必须与 predecessor 连续：第一项 `index == prev_index + 1`，
     之后逐项 +1（用 `checked_add` 防溢出）。跳号 →
     `"log_mismatch: the first entry does not continue the predecessor"` 或
     `"the appended entries are not continuous"`。
   - stale_term / unverified_entry 仍然拒绝整个请求。
2. **完整决策内容判等**：对每个已存在 index 的条目，不再只比 term；先比
   term（不同 → `"log_conflict: the entry at index {i} carries a different
   term"`），再比完整 entry（index/term/kind/payload 全部相等才算重放；
   不同 → `"log_conflict: the entry at index {i} carries a different
   decision"`）。冲突时把**既有记录与来件**两份证据写入
   `node_control_log_conflicts`（同一 `conflict_id` 下 `committed` 与
   `offered` 两行，`UNIQUE(conflict_id, side)`，`INSERT OR IGNORE` —— 重复
   观察同一分歧不重复留证），然后拒绝该请求，**不删除/覆盖已提交事实**。
   完全相同的内容是幂等重放，不是冲突。
   已记录前沿之外的既有分歧按 `log_mismatch` 拒绝，不将未知提交状态的行
   写成 `committed` 冲突证据。
3. **commit_index 不跨洞**：apply 循环只允许新条目恰好扩展耐久记录的
   提交前沿（`entry.index == next`；next 从已记录的 commit_index + 1 开始，
   仅在插入新条目后前进），否则 `log_mismatch`。
   集成审查删除了原候选的 `contiguous_commit_end` 推断：前沿以上有物理行
   不等于那些行已经提交，纯重放不能返回一个未写回的更大提交位置。旧版
   `PushConfig` 留下的稀疏或回退前沿须显式对账，不自动补造历史或重放较早
   的配置效果。`a_replay_does_not_promote_rows_above_the_recorded_commit_frontier`
   覆盖这一边界；父级纠正后的运行证据单独记录，不由原切片的通过结果代替。
4. **附带修复（同一 commit，issue #275 现场证据）**：`election_role` 用
   `get_state::<String>` 之后再 `serde_json::from_str::<ElectionRole>` 解析
   裸字符串，等于双重解码——任何已提交 role 都让 `election_status` 返回
   `Error: expected value at line 1 column 1`，并把 Node 标记为 `failed`
   （`handle` 看到 `serde_json::Error` 置 `failed`）。本 worktree 用真实
   `shenko-node` 二进制复现：`campaign` 之后 `election_status` 报错。
   改为 `get_state::<ElectionRole>` 直接解码（`election.rs` 原 `:747`）。

## 存储 schema

新增表 `node_control_log_conflicts`（持久冲突证据）：
`STORAGE_SCHEMA` 7→8（`lib.rs:32`），迁移 `if locked <= 7` 执行
`election::CONTROL_CONFLICT_SCHEMA`（`lib.rs` 迁移段 ~`:430-435`），
`validate_installation` 在 `version >= 8` 时把该表纳入指纹比对
（`lib.rs` ~`:1187-1189`）。纯新增，既有表不变。

## 机制测试（无外网、无凭据；`crates/shenko-node/src/election.rs` 内 `#[cfg(test)] mod tests`）

真实节点 SQLite fixture（`Node::open` + `Request::Initialize` +
`ensure_member_configuration`）；`primary_entry` 构造带真实 quorum 证书的
`Primary` control entry（certificate voters ⊆ members，单成员域多数=1）。

- `an_append_that_skips_the_predecessor_is_a_log_mismatch_and_changes_nothing`：
  `prev_index=1, entries=[index 3]` → `accepted=0`，reason 以 `log_mismatch`
  开头，`commit_index` 仍为 1；`control_entry(3)` 不存在、日志长度 1、
  `committed_primary_claim` 为 None、term 仍为 1、无冲突证据、
  `this_node_is_primary=false`、配置 revision 仍为 1。
- `a_divergent_entry_is_refused_and_both_records_are_retained`：
  先提交 index 2 的 primary（epoch 1），再以同 index/term 提交 epoch 9 的不同
  decision → `accepted=0`，reason 以 `log_conflict` 开头；`control_entry(2)`
  仍是原记录，`committed_primary_claim.epoch` 仍为 1；
  `control_log_conflicts()` 返回 2 行，同一 `conflict_id`，`committed` 行内容
  等于原记录、`offered` 行内容等于来件；重复同一请求不重复留证。
- `an_identical_replay_is_idempotent_not_a_conflict`：重放同一条 →
  `accepted=0`，reason `"accepted"`，日志长度 2、commit_index 2、无冲突证据。
- `a_replay_does_not_promote_rows_above_the_recorded_commit_frontier`：模拟旧
  writer 留下的记录前沿回退；重放不抬高返回的提交位置，不在既有行后隐式
  扩展新尾部。
- `a_contiguous_suffix_is_accepted_and_the_commit_index_advances`：
  `prev_index=1, entries=[index 2, index 3]` → `accepted=2`，`commit_index=3`，
  primary claim epoch=2。

## 配置 push 的对应写入约束

集成审查进一步确认 `bin/shenko/src/node_sync.rs` 的两个真实生产者均发送
`control_log()` 完整日志；不需要稀疏配置条目的兼容路径。接收端现在要求
非空的连续 `1..N`，且 N 覆盖已记录提交前沿与最高保留物理行；所有条目的
证书、term 顺序与已保留字节在写入前统一核验。已提交前缀中缺少的字节、
分歧与较短历史均拒绝，不凭配置 revision 创造日志位置。

被宣告的配置 revision/digest/term 必须出现在验证过的配置日志条目中；
不再先插入配置，再用刚插入的值证明自己。仅新插入条目应用效果，重放
保持原事实。配置 revision 相同也可以接收更新的 primary 后缀，节点 term
跟随已验证的日志；不能因 revision 相同而跳过真实 catch-up。新学到的
配置在验证完成后保留来源 `committed_at`，不会改写已经保留的配置元数据。

三个新增行为用例分别覆盖缺口拒绝、无对应日志的配置宣告拒绝，以及完整
catch-up、重放与短前缀拒绝的连续过程。原跨进程 `partition_rejoin` 用例
同时要求双方持有相同配置，能检出来源提交时间被本地时间替换的问题。

## 原切片的复现与判定命令（容器 `shenko-dev-persistent-20261026`）

```
export PATH=/usr/local/cargo/bin:$PATH
export CARGO_TARGET_DIR=/var/idw-target
cargo test --offline --locked -p shenko-node --lib -j4
# 结果：28 tests — 27 passed, 1 failed
#   4 个新机制测试全部通过；唯一失败
#   units::tests::generic_trials_export_with_original_plans_without_inheriting_readiness
#   为环境问题：容器在 NFS 上以 root-squash 身份运行，无法在宿主机 worktree 的
#   build/unit-provenance 下创建目录（Os code 13 Permission denied），与本次
#   改动无关。以 SHENKO_COMPAT_EVIDENCE_ROOT=/tmp/unit-provenance-override 重跑
#   该测试单独通过。
cargo test --offline --locked -p shenko-node --test partition_rejoin
# 2 passed, 0 failed（需要上面的 election_role 修复；修复前 main 上该测试在
# ElectionStatus 处会报错）
cargo test --offline --locked -p shenko-node --test cognition_report --test reconciliation_receipts --test unit_uninstall_wire
# 3 + 1 + 1 = 5 passed, 0 failed（schema-8 迁移在真实节点进程中工作正常）
```

真实二进制复现修复前 bug（用于判定 election_role 缺陷）：

```
echo -n '{"command":"campaign"}' | shenko-node <dir>   # OK，term=2
echo -n '{"command":"election_status"}' | shenko-node <dir>
# 修复前：Error: expected value at line 1 column 1（且节点被标记 failed）
# 修复后：返回完整 election_status JSON
```

## 父级最终验证

最终源码的 `election.rs` SHA-256 为
`2b31ff75f577c24ba61f635a64f2b91863c3acd9a032bbffaca746990acf5049`。
容器内使用 `--offline --locked -j2`，逐条命令保留实际退出状态和运行前后
源码哈希：

- `cargo test -p shenko-node --lib election::tests`：14 passed，0 failed。
  Cargo 的子串过滤还匹配了 6 条 `selection::tests`；8 条控制日志用例均通过。
- `cargo test -p shenko-node --test partition_rejoin --test cognition_report --test reconciliation_receipts --test unit_uninstall_wire`：7 passed，0 failed。
- `cargo check -p shenko`：exit 0，覆盖 CLI 对本次 Broker/Node 合同的消费者。

原始日志与 receipt 已通过原容器与归档端的 SHA-256 比对，位置为
`build/verification-retained-20261111-cb466628/manifest-final.json`。归档也保留
了较早的回归 fixture 编译失败，以及跨进程检查发现来源提交时间被改成
本地时间的失败；最终结果不由这些旧运行代替。R4 的 32 条库测试通过发生在
时间元数据修正之前，最终相关路径由 R5 的上述检查覆盖。

## 本文件不证明什么

- 不证明"全部共识/认证协议已验收"：证书与配置的密码学绑定（voter 身份、
  config_digest 的跨节点一致性、证书伪造抵抗）继续需要独立审查；本修复只
  保证 follower 复制路径的前缀连续与内容判等。
- 配置 push 的历史字节缺口不自动补造。完整且匹配的显式 push 可以证明旧
  前沿以上保留行所属的真实提交前缀；内容不匹配或已提交字节缺失仍须按
  分支对账处理，不覆盖旧事实。
- 机制测试在单节点私有 SQLite 上进行，不含真实 SSH 传输、不含多进程并发、
  不含宕机中点恢复；`partition_rejoin` 集成测试用独立进程验证了同一
  append/election_status 路径。
- `node_control_log_conflicts` 是 schema-8 新增表：迁移只执行
  `CREATE TABLE`（无数据回填），旧安装升级后该表从 0 行开始——旧安装
  此前发生的分歧不会被追溯留证。
- 提交到本分支的 commit 只是 slice 产出；SPEC §57.14 的实现状态与
  `docs/requirements/acceptance-cases.json` 的验收行由集成负责人统一写，
  本文件不构成 verified_real 声明。
