# 长期运行观察：7/30 天证据采集与报告

C19/#24 与 [COMPLETION](../governance/COMPLETION.md) [§2](../specs/architecture.md#spec-2)「长期性」要求 S5 的 7/30 天报告按真实墙钟时间和明确候选/环境形成，加速测试不能冒充 30 天无人运行。本页描述 `scripts/observe_longrun.py`——一个只做采样的开发工具：它把可归属的运行观察写到磁盘、把采样与声明窗口分开，并把没跑够的窗口永远标 `unobserved`。它不是产品功能，不改变主体行为；它只是让长跑证据可形成、可对账。

## 它能采集什么

对一个明确候选（commit/二进制 hash）、明确运行环境（host/OS/备注）和明确 subject（固定 PID 或监督管理的 pid-file）：

- 每个采样点：主进程 RSS/VmHWM/VmSize/线程数/FD 数/存活状态/自身 uptime，可选伴随进程，宿主 loadavg/MemAvailable/数据盘余量。`Z`/`X`/`x` 进程不是存活主体，退出后不再累计 live-process uptime。
- 段边界：当主进程的 `(boot_id, pid, starttime, comm)` 指纹变化时写一条 `segment` 记录并给出原因（`starttime_changed_same_pid`、`pid_changed`、`boot_id_changed`、`reappeared`、`comm_changed`）。先区分换 PID，再判断同 PID 的 starttime 变化。boot ID 来自 `/proc/sys/kernel/random/boot_id`。
- 重启记录通过 `sample_seq` 绑定后续样本；边界落盘但样本未写入时，重试同一转换复用已有边界。报告只计入样本佐证的唯一转换，孤立边界单列为完整性缺口，不当成已观察重启。
- 采样器自身停机与断电：墙钟或同 boot uptime 的相邻间隔超过 manifest 的 `interval_seconds × gap-factor`，计为 unobserved 秒数，**整次 run 的全部声明窗口不合格**，不能把断开的跨度拼成 7/30 天。`--gap-factor` 只允许 1–3（默认 3），不能在报告时放宽到任意停机长度。
- 损坏日志：无法读取的行计入 `unreadable_bytes`，无换行结尾的尾部另计 `unterminated_tail_bytes`，即使尾部恰好是合法 JSON 也不合格。继续采样会在任何追加之前拒绝，不修改或丢弃旧字节；保留目录供审计，另开新 run。

每条记录是 `records.jsonl` 的一行，追加后先 `fsync` 文件再 `fsync` 所在目录（包括第一次创建日志和重试）；manifest 与目录也 `fsync`。manifest 在 `init` 时冻结（包含声明窗口与候选身份）。一目录只绑定一个 run：必须有且只有一个位于记录开头、schema/manifest hash/recorder 身份均匹配的 `run_header`。缺 header、别的 run_id、未知/损坏记录、缺失/重复序号、未核验的存活主体指纹都不能准入；采样前发现这些问题会拒绝继续。manifest 改动产生 `manifest_drift`，全部窗口降为 `unobserved`，不会补写 header 将旧样本重新绑定。

初始化在写 manifest 前从实际 run 目录向上逐层 fsync 到根，持久化每一层新建目录在父目录中的入口；已有目录也同步，以覆盖先前失败初始化留下但未确认持久的入口。此协议检查仍不等于物理断电证明。

记录格式是 **schema 3**：同 kernel boot 的 uptime 佐证、段边界的 `sample_seq`、实测环境、逐样本二进制漂移与 outage 记录。schema 1/2 的记录缺少这些证据，明确拒绝，不进行推断式升级；原字节保留。不同候选/闭包、recorder 变更或已损坏证据均使用新目录和新 `run_id`。

报告文档是 **schema 5**：保留 `lifecycle`（是否封存、`stop_reason`、停止样本与 checkpoint 列表），并要求每个存活样本的实际执行文件 SHA-256。采样器打开 `/proc/<pid>/exe`，从同一文件描述符读取字节与元数据；不重新打开 readlink 给出的可变路径，也不以相同长度和秒级修改时间认定产物相同。无法测量的身份保持未知并使窗口不合格。只有 `run_end` 且 `stop_reason=completed` 的报告能成为时长证据；未封存或提前停止的报告保留字节与其跨度，但不构成已完成的窗口。

运行有生命周期：

- `checkpoint --id <name>` 把 durable 复查点绑定到**已经写入**的最后一个样本，不能指向尚未采到的样本；重复 id 或 `sample_seq` 与最后样本不符会成为完整性缺口。
- `stop --reason completed|aborted|superseded|candidate_replaced|failed` 只封存一次，并绑定已写入的样本；`run_end` 的 `sample_seq`/`samples` 必须与该 run 实际写入的样本一致，否则同样是完整性缺口而不是封存。
- 封存或字节已损坏的 run 拒绝 `sample`/`checkpoint`/`stop` 追加：已准入的窗口不会在事后被同一 run 的新记录改写或作废。

消费侧 `shenko-evidence --long-run PATH=SHA256` 只准入这种 schema 5 报告：调用方摘要、40 位候选提交、实测二进制摘要、profile、`integrity.intact`、执行文件验证样本列表、封存状态、停止样本绑定与逐窗口合格秒数都要成立。缺失执行文件摘要、只有相同长度/mtime 或旧 schema 4 的报告不能借用新合同；旧字节保留且拒绝原因明确。被拒绝的报告仍把声明窗口留在分母里（`unobserved`），拒绝原因进 `rejections`；多个已准入报告只有二进制与 profile 完全一致时才合并窗口，否则 `identity_conflicts` 显式列出、`duration_status` 保持 `unobserved`。只有名为 `7d`（≥604800 秒）与 `30d`（≥2592000 秒）的窗口都合格时 `duration_status` 才是 `observed`；把一分钟写成 `7d` 会被拒绝，只声明其他窗口（如 `demo`）的报告只能证明那个窗口。

## 用法

```sh
# 1. 冻结声明：候选、环境、要声明观察的窗口、subject 来源
python3 -I scripts/observe_longrun.py init \
  --dir /path/run-dir --run-id s5-candidate-YYYYMMDD \
  --workload "shenko start C=1 + NapCat" \
  --candidate-commit <git sha> --binary-path <实际运行的二进制> \
  --binary-sha256 <sha> --profile <runtime/build profile> \
  --evidence-class real_observation \
  --host <host> --os <kernel> \
  --window 7d=604800 --window 30d=2592000 \
  --interval 300 --pid-file /path/subject.pid

# 2a. 外挂调度（cron/systemd timer/tmux 循环）每次一条样本：
python3 -I scripts/observe_longrun.py sample --dir /path/run-dir

# 2b. 或进程内循环（b 不保证采样器存活，但停机本身会被记为 gap）：
python3 -I scripts/observe_longrun.py run --dir /path/run-dir \
  --deadline-seconds 604800

# 2c. 复查点与封存（每步一条，封存后不再接受记录）：
python3 -I scripts/observe_longrun.py checkpoint --dir /path/run-dir \
  --id review-1 --note "第一次长期复查"
python3 -I scripts/observe_longrun.py stop --dir /path/run-dir \
  --reason completed --note "7d 窗口达到"

# 3. 汇总：窗口没跑够就是 unobserved；CI 可用退出码门禁
python3 -I scripts/observe_longrun.py report --dir /path/run-dir \
  --require-observed --write

# 4. 义务报告消费报告（只读；绑定调用方期望的摘要）
shenko-evidence --spec docs/contracts.json --evidence-dir docs/evidence \
  --candidate-commit <sha> --long-run /path/run-dir/reports/report-<stamp>.json=<sha256> \
  --json build/spec-obligation-report.json
```

`checkpoint`/`stop` 在字节损坏或 run 已封存时退出 2 并保留原字节。`report` 对未封存的 run 仍给出跨度与 `unobserved` 结论，不推断完成。
systemd timer 例：`*-*-* *:0/5` 调 `sample`。采样器可以死在任意时刻；`report` 通过样本间隔与 seq 连续性如实报告缺口。`report --write` 把报告落到 `DIR/reports/` 供证据归档。

## 诚实性边界（重要）

- **采样即采样**：两个样本之间主体的行为不被证明；RSS 漂移、宕机分布以样本为准，不插值。
- **uptime 是样本份额**：`subject_sample_uptime_fraction` 是「看到存活的样本占比」，不是精确停机时间；两次采样间死而复生只见于 `segment` 边界。
- **重启检测有条件**：同一 PID 的 starttime 变化可检出；换 PID 的重启只有当新 PID 经由 `--pid` 或 pid-file 给出后才被观察到（在此之前样本记录为 absent）。PID 复用在同一 boot 内理论上可漏检（starttime+comm 指纹缓解但不完美）。无法读取 boot ID/starttime 的存活样本不具有合格身份，不用于窗口准入。
- **时间需要佐证**：`observed_span_seconds` 只展示墙钟端点差，不是合格覆盖。`qualifying_span_seconds` 在完整、无已知 gap 的 run 中累计相邻墙钟差与同 boot uptime 差的较小值；前跳不能凭空增加时间，回跳/非递增时间、无法读取时钟或跨 boot 一律不合格。本工具保守拒绝跨宿主重启的窗口，不能用 epoch 猜测重启期间的连续性。
- **调度器不保证采样器存活**：`run` 模式随进程退出而停止；`sample` 依赖外部调度。采样器停机期间不生成记录，缺口如实报告。
- **声明≠结果**：manifest 的窗口是声明的观察义务；只有合格覆盖 ≥ 窗口才标 `observed`。已知 gap 或任何上述完整性/绑定缺口使整次 run 的合格覆盖为零；更长的端点跨度不能抵消。`--require-observed` 在任一窗口未合格时退出 3；非法输入/被拒绝的追加退出 2。
- **证据并非认证**：每个目录只运行一个写入者；没有并发写入锁，交错写入/重复序号将使证据不合格。本地 JSONL 不带抗篡改签名，manifest 的候选元数据由调用者提供，不证明 PID 实际执行了该二进制。fsync 调用路径不是物理断电耐久性证据。
- **未见清单**：真实断电 RPO、音频端到端时延、工具/FTS p95、路由器/Android profile、多节点长跑、断电/满盘/分区故障注入都未由本工具观察；它们需要各自的故障注入与设备测量，见 [§23](../specs/verification.md#spec-23) 尾部与本页「仍未观察」。

## 仍未观察到的义务

| 义务 | 状态 |
| --- | --- |
| 任何 7/30 天实际窗口 | 没有任何候选长跑结果；当前为 schema 3 记录/schema 5 报告，长跑 run 仍需绑定实际交付候选与 profile、实际执行文件摘要并经过真实墙钟窗口。短时工具核验不替代这些窗口。 |
| kernel/rescue 常驻 RSS ≤16 MiB、静止 ≤64 MiB（[§22](../specs/nodes.md#spec-22) 拟定目标） | 未测量；采样拿到实际值后再定值/裁剪。 |
| 路由器 64 位/512 MB profile、Android profile、多节点长跑 | 未开始；需要真实设备与部署。 |
| 主体行为正确性（持续思考、消息不丢） | 本工具只看进程资源与存活，不看语义；行为验收仍须产品侧协议证据。 |

## 自检

`python3 -I scripts/observe_longrun_test.py` 包含 **15 个独立永久场景**：完整窗口精确边界、30 天端点之间的已知采样缺口、缺失 header、撕裂追加尾部、kernel boot-id 路径、换 PID 后样本写入失败再重试、创建日志时目录 fsync、zombie 主体、样本序号跳变、封存生命周期、checkpoint 绑定、封存样本绑定、等长同时间但字节不同的产物、reported pathname 与实际打开执行文件的区别、以及身份未知的真实观测形状不能合格。合成时钟与进程形状只核验工具合同，不是产品长跑。历史 source `17ccbd92` 的九场景与 procfs 短时 smoke 仍只证明原版本，回执为 `build/c19-observation-native-20261029/native-17ccbd92-67b907cd/summary.json`；当前 schema 5 的核验以父级新回执为准，不能沿用旧报告升级结论。真实 7/30 天和物理断电均未获证明。
