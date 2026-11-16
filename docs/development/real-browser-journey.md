# 同一真实单机旅程的浏览器观察

这是 `scripts/real_single_node_journey.py --browser` 的运行说明，**不是通过收据**。
切片以 `1329627` 为源码基线，保留既有真实模型、有限 child、E1、持久输出、原输入
幂等重放、同一 subject/branch 和重启继续工作的所有成功条件。浏览器是这条旅程的
真实输入者和观察者，不另建聊天主体或另跑一组模型场景。

相关合同：[Web 界面](../specs/web-interface.md)、[流式阅读与验收边界](../specs/web-interface.md#spec-21-11)。

## 运行条件与固定依赖

使用实际候选 CLI、实际 OpenAI-compatible 模型服务及全新证据目录。CLI 自己在同一
runner 的 loopback 提供嵌入前端，浏览器打开这个 live 页面。已经停止的进程、历史
artifact、静态网页或重建的 SQLite 数据不能作为该旅程的服务。

Python 依赖固定为 `websockets==16.0` 和 `playwright==1.62.0`。后者也是开发环境已安装
的版本；其[微软官方 release](https://github.com/microsoft/playwright-python/releases/tag/v1.62.0)
与 [PyPI 发布页](https://pypi.org/project/playwright/1.62.0/) 指向 Chromium 151.0.7922.34。
使用包所匹配的 Chromium 和 `channel="chromium"`，不选择浮动的系统 Chrome。
浏览器二进制与系统依赖按[官方安装说明](https://playwright.dev/python/docs/browsers)取得；
执行时另记实际浏览器版本、可执行文件 SHA-256、Python、系统和页面 viewport。

```sh
python3 -m venv .ci/python
.ci/python/bin/pip install websockets==16.0 playwright==1.62.0
.ci/python/bin/python -m playwright install --with-deps --no-shell chromium

.ci/python/bin/python scripts/real_single_node_journey.py \
  --binary /absolute/path/to/candidate/shenko \
  --source-revision "$(git rev-parse HEAD)" \
  --model-url http://127.0.0.1:18832 --model <actual-served-model-id> \
  --browser --timeout 1800 --evidence-dir /absolute/new/evidence-directory
```

`--timeout` 仍是原旅程的每阶段观察上限，不改变产品封存的请求期限。若明确配置
`SHENKO_LOCAL_MODEL_REQUEST_TIMEOUT_MS`，继续由真实 provider binding 与原有 manifest
记录。命令不会为浏览器额外发出模型探测或生成请求。

`.github/workflows/real-single-node.yml` 在已有显式真实模型 job 中安装上述依赖并传入
`--browser`。已有 workflow opt-in、模型版本与散列、原 ledger 检查、失败退出和证据
保留规则继续生效。

## 一个连续旅程

1. 原 harness 启动候选 CLI，并从真实 `/api/self` 取得 subject。浏览器打开同一服务，
   必须实际看到 live shell、输入区及服务端 hello。记录服务实际提供的前端 build manifest。
2. 使用页面的“向同一主体追加输入”输入框和 Enter 提交原有限任务。页面自己生成随机
   submission/message ID；观察器从真实 `framesent` 取得它们，再关联这个连接实收的
   durable receipt。没有脚本注入 WebSocket、localStorage、UI 状态或成功回执。
3. 原 harness 继续只读同一 Store/CAS，并执行原有模型 child、`host.printf` child、E1
   和结果之后的已提交输出断言。有合格结果时，浏览器通过实际 Inspect 控件打开与该
   step ID 对应的已提交正文，观察原标记与计算结果；不会用输入框里的同文内容代替回复。
4. 回到对话，在真实输入框留下未发送草稿。只有已有内容确实足够滚动，才用鼠标滚轮
   建立中段阅读位置。保持同一页面，原 harness 停止并重启同一 CLI/data_dir。
5. 重连必须关联实际停止边界和原 socket 的 close，之后再观察另一个 socket 的 hello。
   若先前存在已提交 part，还必须收到该新连接的真实 committed replay。检查草稿、
   已聚焦输入框和边界前已提交事实；仍在增长的流不被当作不可变化的历史。
6. 原观察 socket 原样重放页面第一次提交的身份与 payload，保留既有服务端幂等断言。
   新的“继续工作”输入仍经实际 composer。原有重启继续、结果保留和 branch 断言不变。
   继续工作的真实输出出现后，再在同一页面复核原稳定消息的正文和唯一性，覆盖迟到 replay。

`scripts/real_browser_witness.py` 只是该旅程的页面观察器。其 DOM evaluation 全部是读取；
输入、视图切换和滚动都通过实际页面操作。原有不带 `--browser` 的公共 WebSocket
旅程仍可单独诊断后端，但其通过不构成浏览器证据。

## 留存与判断范围

证据根仍保留原 manifest、输入、原始 WebSocket、ledger/CAS 和失败日志；`browser/`
追加以下材料：

- `environment.json`、服务实际返回的前端 manifest 及其散列。
- `events.jsonl`：带序号、相对时间和 socket ID 的实际发送/接收帧、close、请求失败和页面错误。
- 已触达阶段的 PNG、DOM 读数和 ARIA 快照；页面关闭前尝试保存最终状态。
- `summary.json`：每项实际检查、未触达原因、原始页面错误及清理失败。上下文、浏览器、
  driver 都由同一观察器关闭；清理或证据写入失败保留为失败，不返回绿灯。

浏览器检查并入原旅程的总结果，原 ledger 检查没有放宽。焦点必须在停止边界确实存在，
才检查恢复后是否保留。阅读锚点必须来自已提交消息或本页面原输入，且实际处于中段，
才记录静态 viewport 下的位移与 2 CSS px 门槛。没有足够真实内容时记录 `observed:false`，
不伪造长文本或把缺测记为通过。初次 replay 后及继续工作后的消息一致性分别留存。

`stream_part` 只有实际接收到才计数；即使计数大于零，也不证明长流阅读质量或延迟指标。
ARIA 快照和键盘输入不是屏幕阅读器验收。当前固定 1280×900、浅色、reduced motion
的 Chromium 观察，不代表窄屏、200% 缩放、真实移动设备、所有 UI-S01–10、真实渠道、
NapCat/coding seat、物理故障或 7/30 天运行已经完成。

## 本切片的验证状态

编写时已完成 Python 语法检查、命令帮助读回和补丁空白检查。AST 比对确认
`evaluate_first`、`continued`、`snapshot` 与基线完全相同；这只证明断言代码未被改写。
S2 分工进行了独立只读审查，补上了停止/新连接的精确关联及迟到 replay 的第二次核对。

尚未在本切片执行真实浏览器/模型旅程。当前本机可复用的 Playwright 包存在，但没有
观察到匹配浏览器 cache，且共享磁盘剩余空间不足以安全安装浏览器。未下载或运行另一份
浏览器来冒充候选结果。后续以命名候选的真实 job、原始日志和 `browser/summary.json`
更新结果；本页不将已有纯 WebSocket 或历史 UI 证据改称本切片的通过结果。
