# 长期运行观察：7/30 天证据采集与报告

C19/#24 与 [COMPLETION](../governance/COMPLETION.md) §2「长期性」要求 S5 的 7/30 天报告按真实墙钟时间和明确候选/环境形成，加速测试不能冒充 30 天无人运行。本页描述 `scripts/observe_longrun.py`——一个只做采样的开发工具：它把可归属的运行观察写到磁盘、把采样与声明窗口分开，并把没跑够的窗口永远标 `unobserved`。它不是产品功能，不改变主体行为；它只是让长跑证据可形成、可对账。

## 它能采集什么

对一个明确候选（commit/二进制 hash）、明确运行环境（host/OS/备注）和明确 subject（固定 PID 或监督管理的 pid-file）：

- 每个采样点：主进程 RSS/VmHWM/VmSize/线程数/FD 数/存活状态/自身 uptime，可选伴随进程，宿主 loadavg/MemAvailable/数据盘余量。
- 段边界：当主进程的 `(boot_id, pid, starttime, comm)` 指纹变化时写一条 `segment` 记录并给出原因（`starttime_changed_same_pid`、`pid_changed`、`boot_id_changed`、`reappeared`），重启不会偷偷续接上一段历史。
- 采样器自身停机与断电：样本间隔超过 `--interval × gap-factor` 计为 unobserved 秒数；日志尾部不完整行计为 `unreadable_bytes`，不静默跳过也不修复。

每条记录是 `records.jsonl` 的一行，追加后 `fsync`；manifest 与目录也 `fsync`。manifest 在 `init` 时冻结（包含声明窗口与候选身份），采样后有改动会产生 `manifest_drift`，并把全部窗口降为 `unobserved`——不同的候选/闭包必须开新 `run_id`，不能把不同候选的时间拼成一个稳定长跑。

## 用法

```sh
# 1. 冻结声明：候选、环境、要声明观察的窗口、subject 来源
python3 -I scripts/observe_longrun.py init \
  --dir /path/run-dir --run-id s5-candidate-YYYYMMDD \
  --workload "shenko start C=1 + NapCat" \
  --candidate-commit <git sha> --binary-sha256 <sha> \
  --host <host> --os <kernel> \
  --window 7d=604800 --window 30d=2592000 \
  --interval 300 --pid-file /path/subject.pid

# 2a. 外挂调度（cron/systemd timer/tmux 循环）每次一条样本：
python3 -I scripts/observe_longrun.py sample --dir /path/run-dir

# 2b. 或进程内循环（b 不保证采样器存活，但停机本身会被记为 gap）：
python3 -I scripts/observe_longrun.py run --dir /path/run-dir \
  --deadline-seconds 604800

# 3. 汇总：窗口没跑够就是 unobserved；CI 可用退出码门禁
python3 -I scripts/observe_longrun.py report --dir /path/run-dir \
  --require-observed --write
```

systemd timer 例：`*-*-* *:0/5` 调 `sample`。采样器可以死在任意时刻；`report` 通过样本间隔与 seq 连续性如实报告缺口。`report --write` 把报告落到 `DIR/reports/` 供证据归档。

## 诚实性边界（重要）

- **采样即采样**：两个样本之间主体的行为不被证明；RSS 漂移、宕机分布以样本为准，不插值。
- **uptime 是样本份额**：`subject_sample_uptime_fraction` 是「看到存活的样本占比」，不是精确停机时间；两次采样间死而复生只见于 `segment` 边界。
- **重启检测有条件**：同一 PID 的 starttime 变化可检出；换 PID 的重启只有当新 PID 经由 `--pid` 或 pid-file 给出后才被观察到（在此之前样本记录为 absent）。PID 复用在同一 boot 内理论上可漏检（starttime+comm 指纹缓解但不完美）。
- **墙钟依赖**：样本时间来自 `time.time()`；时钟回跳不会伪造覆盖——span 计算直接用 epoch 差，负/异常间隔如实出现在 gap 统计中。
- **调度器不保证采样器存活**：`run` 模式随进程退出而停止；`sample` 依赖外部调度。采样器停机期间不生成记录，缺口如实报告。
- **声明≠结果**：manifest 的窗口是声明的观察义务；`report` 只有在记录跨度 ≥ 窗口且 manifest 未被改动时才标 `observed`。
- **未见清单**：真实断电 RPO、音频端到端时延、工具/FTS p95、路由器/Android profile、多节点长跑、断电/满盘/分区故障注入都未由本工具观察；它们需要各自的故障注入与设备测量，见 SPEC §23 尾部与本页「仍未观察」。

## 仍未观察到的义务

| 义务 | 状态 |
| --- | --- |
| 任何 7/30 天实际窗口 | 工具就绪，没有任何候选长跑结果；首个 run 需绑定实际交付候选与环境。 |
| kernel/rescue 常驻 RSS ≤16 MiB、静止 ≤64 MiB（§22 拟定目标） | 未测量；采样拿到实际值后再定值/裁剪。 |
| 路由器 64 位/512 MB profile、Android profile、多节点长跑 | 未开始；需要真实设备与部署。 |
| 主体行为正确性（持续思考、消息不丢） | 本工具只看进程资源与存活，不看语义；行为验收仍须产品侧协议证据。 |

## 自检

`python3 -I scripts/observe_longrun_test.py`：4 个独立场景覆盖真实 /proc 采样与声明窗口的 honest 边界、重启/absent/断电尾部/manifest 篡改的完整性表达、manifest 不可变与未解析 PID。它不是产品测试，也不证明长跑通过。
