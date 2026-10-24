# 当前社区开发栈

总追踪：[#4](https://github.com/cyjin-yl/shenko/issues/4)。朋友从 [PROMPT](../../../PROMPT.md) 加入已有开发；协调者完整目标在 [GOAL](../../../GOAL.md)。本图不宣称产品完成。

## 如何接续

认领和心跳仅作协调提示，不锁任务。朋友突然下线时，从最后已推checkpoint建 `continue/<work-id>/<session>` 分支继续，保留原作者、原提交和原PR链接，不等交还/释放/审批/在线，不强推对方分支。尚未推送的本地改动由持有者保留，不能假称社区可取。

语义依赖与Git父分支分别表达：实际依赖未合并父PR补丁时，子PR以父分支为base；独立切片base main。父合入后当前整合者整理后继base。没有实际补丁不开空PR；已有权限与足够证据时自主合入，不因等待review停止独立工作。

## 已发布工作包

| 工作包 | 真实 issue | 语义依赖 |
|---|---|---|
| [C01](../../tasks/C01.md) | [#6 持久事实底座：ledger、任务／效果、inbox/outbox与重启恢复](https://github.com/cyjin-yl/shenko/issues/6) | 可直接开始 |
| [C02](../../tasks/C02.md) | [#7 持续主体与真实children：串行决策、语义等待、委派与双向续接](https://github.com/cyjin-yl/shenko/issues/7) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C04](https://github.com/cyjin-yl/shenko/issues/9) |
| [C03](../../tasks/C03.md) | [#8 真正的cordis-rs插件主干、版本化hook与服务生命周期](https://github.com/cyjin-yl/shenko/issues/8) | 可直接开始 |
| [C04](../../tasks/C04.md) | [#9 真实推理broker：并发公平、KV与provider wire合同](https://github.com/cyjin-yl/shenko/issues/9) | [C01](https://github.com/cyjin-yl/shenko/issues/6) |
| [C05](../../tasks/C05.md) | [#10 真实WASM家园、挂载、包闭包与可恢复首启](https://github.com/cyjin-yl/shenko/issues/10) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C03](https://github.com/cyjin-yl/shenko/issues/8) |
| [C06](../../tasks/C06.md) | [#11 具名WASM工具移植：toybox/gcc、认证Git/gh/tea、SSH](https://github.com/cyjin-yl/shenko/issues/11) | [C05](https://github.com/cyjin-yl/shenko/issues/10) |
| [C07](../../tasks/C07.md) | [#12 远程执行与现有coding seats：SSH身份、TUI、OMP与Claude](https://github.com/cyjin-yl/shenko/issues/12) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C03](https://github.com/cyjin-yl/shenko/issues/8) |
| [C08](../../tasks/C08.md) | [#13 首版自开发闭环：NapCat→主→child→seat→候选验证](https://github.com/cyjin-yl/shenko/issues/13) | [C02](https://github.com/cyjin-yl/shenko/issues/7), [C03](https://github.com/cyjin-yl/shenko/issues/8), [C05](https://github.com/cyjin-yl/shenko/issues/10), [C07](https://github.com/cyjin-yl/shenko/issues/12), [C09](https://github.com/cyjin-yl/shenko/issues/14) |
| [C09](../../tasks/C09.md) | [#14 记忆与研究：原始真相、Git知识、PMA与reset-to-best](https://github.com/cyjin-yl/shenko/issues/14) | [C01](https://github.com/cyjin-yl/shenko/issues/6) |
| [C10](../../tasks/C10.md) | [#15 Awareness／感知／Skill依赖与自解释、生态接入](https://github.com/cyjin-yl/shenko/issues/15) | [C03](https://github.com/cyjin-yl/shenko/issues/8), [C09](https://github.com/cyjin-yl/shenko/issues/14) |
| [C11](../../tasks/C11.md) | [#16 正式WebUI：Proto-UI/shadcn/compiler seam与真实流式时间线](https://github.com/cyjin-yl/shenko/issues/16) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C02](https://github.com/cyjin-yl/shenko/issues/7), [C03](https://github.com/cyjin-yl/shenko/issues/8) |
| [C12](../../tasks/C12.md) | [#17 对话配置与持久全栈Playground](https://github.com/cyjin-yl/shenko/issues/17) | [C05](https://github.com/cyjin-yl/shenko/issues/10), [C10](https://github.com/cyjin-yl/shenko/issues/15), [C11](https://github.com/cyjin-yl/shenko/issues/16) |
| [C13](../../tasks/C13.md) | [#18 渠道与联系人：NapCat/napcat-cli、官方QQ、iLinkBot、飞书](https://github.com/cyjin-yl/shenko/issues/18) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C02](https://github.com/cyjin-yl/shenko/issues/7), [C03](https://github.com/cyjin-yl/shenko/issues/8) |
| [C14](../../tasks/C14.md) | [#19 Python／Bun／Node真实WASM REPL与恢复](https://github.com/cyjin-yl/shenko/issues/19) | [C05](https://github.com/cyjin-yl/shenko/issues/10), [C06](https://github.com/cyjin-yl/shenko/issues/11) |
| [C15](../../tasks/C15.md) | [#20 OS与VM节点：LayerFS、Computer Use、Android及可选BoxLite](https://github.com/cyjin-yl/shenko/issues/20) | [C05](https://github.com/cyjin-yl/shenko/issues/10), [C07](https://github.com/cyjin-yl/shenko/issues/12) |
| [C16](../../tasks/C16.md) | [#21 声音与共同活动：通话、旁听、转写修订与声纹](https://github.com/cyjin-yl/shenko/issues/21) | [C02](https://github.com/cyjin-yl/shenko/issues/7), [C13](https://github.com/cyjin-yl/shenko/issues/18) |
| [C17](../../tasks/C17.md) | [#22 Shenko网络：部署加入、异构智能、分区选主与无LLM复活](https://github.com/cyjin-yl/shenko/issues/22) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C02](https://github.com/cyjin-yl/shenko/issues/7), [C04](https://github.com/cyjin-yl/shenko/issues/9), [C07](https://github.com/cyjin-yl/shenko/issues/12) |
| [C18](../../tasks/C18.md) | [#23 云周期额度：观测、预测、模型路由与准入节奏](https://github.com/cyjin-yl/shenko/issues/23) | [C04](https://github.com/cyjin-yl/shenko/issues/9), [C10](https://github.com/cyjin-yl/shenko/issues/15) |
| [C19](../../tasks/C19.md) | [#24 全范围证据／性能／文档与7/30天持续交付收敛](https://github.com/cyjin-yl/shenko/issues/24) | 可直接开始 |
| [C20](../../tasks/C20.md) | [#25 两个独立compact正式插件：budget与snapcompact](https://github.com/cyjin-yl/shenko/issues/25) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C03](https://github.com/cyjin-yl/shenko/issues/8), [C04](https://github.com/cyjin-yl/shenko/issues/9), [C09](https://github.com/cyjin-yl/shenko/issues/14) |
| [C21](../../tasks/C21.md) | [#26 运行中自升级与救援：A/B迁移、epoch交接与事实保留](https://github.com/cyjin-yl/shenko/issues/26) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C03](https://github.com/cyjin-yl/shenko/issues/8), [C05](https://github.com/cyjin-yl/shenko/issues/10), [C08](https://github.com/cyjin-yl/shenko/issues/13) |
| [C22](../../tasks/C22.md) | [#5 可自定义人格／灵魂／实例姓名：移除品牌身份硬编码](https://github.com/cyjin-yl/shenko/issues/5) | [C01](https://github.com/cyjin-yl/shenko/issues/6), [C03](https://github.com/cyjin-yl/shenko/issues/8) |

## 近期主线

优先 [#6](https://github.com/cyjin-yl/shenko/issues/6) 恢复正确 → [#12](https://github.com/cyjin-yl/shenko/issues/12) 真实远端执行 → [#22](https://github.com/cyjin-yl/shenko/issues/22) 第二节点加入并接走一项持久任务。人格独立工作包为 [#5](https://github.com/cyjin-yl/shenko/issues/5)。

## 分母与证据

[机器可读图](community-stack.json) 映射全部147 R、55 B、216 case；R有唯一主负责包，B可跨包，case主负责包组织组合验证。归属不等于实现/适用性裁定/验收通过。完整S1–S5、具名WASM目标和7/30天真实观察全部保留；不得借任务拆分缩小范围。

PR #1–#3已合并。新工作包的代码PR尚未按本图建立；本图的branch/base是开工约定，不是虚构已开stack。当前main盘点基线 `d4b612b`，未整合候选见各任务卡，开工重查Git真实状态。
