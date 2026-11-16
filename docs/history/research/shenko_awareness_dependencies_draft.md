# §45 增量：可安装单元与依赖装配

状态：可并入 §45 的规范草案，未实现。依据 U48 与既有 §9 包闭包、§37 Hook、§41 持久调用及 §45 能力合同；本轮无新增上游研究。本节补充并细化 §45.4、§45.6，不另设 awareness 管理器。

## 独立单元及其职责

**感知、直觉、Skill 与兑现能力的代码都可以按模块粒度独立安装、查阅、更新和移除。** 每个单元具有稳定身份、版本或 revision、内容摘要、适用范围与可读说明；可以由一个发行包交付，也可以跨包组合。一个包包含多个单元不抹去这些单元各自的身份与依赖。无需为纯知识单元制造一个常驻进程或独立 Cordis 插件实例。

安装的是感知定义／实现、直觉材料或实现等工件；它们产出的观测事实与运行时判断是另有出处的记录，不随单元卸载而删除。

| 单元类别 | 提供什么 | 不意味着什么 |
| --- | --- | --- |
| Perception／感知 | 将观察所得状态表达为有来源、对象、时间及版本的事实，例如“连接 X 的 WebSocket 已关闭” | 不把“此连接断开”直接说成“QQ 全部不可用”，也不自行判定根因 |
| Intuition／直觉 | 作为 awareness 的可安装材料或实现，关联情境、知识与可能应关注的对象，例如“先找到承载当前连接的 provider” | 不等于已执行诊断、必然正确的结论或新增操作授权 |
| Skill | 可查阅的方法、步骤、例子或可执行组合，例如“检查当前 NapCat 连接的诊断信息与登录状态” | 不强制每次按固定步骤执行；文档中出现的工具不自动成为当前可用工具 |
| Code／代码 | 插件、adapter、探针、工具或组合实现，兑现实际读取、解释及执行能力 | 包已下载不证明依赖、服务绑定和实际连接均已可用 |

“原子化”首先指上述单元可以独立识别、装配与演进；不承诺任意下载、安装脚本、远端配置或消息发送能组成可回滚的外部事务。准备完成的本地闭包可以复用 §9 的 generation 原子发布能力。原子发布的是版本选择和绑定快照，运行时重连、进程接管与外部效果继续遵守各自合同。

## 一份依赖图，两条独立的依赖属性

安装对象复用现有 Nix-like 包／unit 引用、内容寻址 store、lock 与 generation。Cordis 装配其中需要执行的 services、events、订阅和 hooks；纯卡片、直觉材料和 Skill 文档由同一能力／知识索引提供查阅，不再建立独立安装系统。

每条依赖分别声明两个轴：

- **强度**：required 是该单元声明的必需依赖；recommended 是可缺省的建议依赖。建议依赖缺失时记录降级或省略原因，不冒称已安装。必需依赖未解析或不可用时，不发布“相关能力已经就绪”的结论；受影响单元可保持未启用，原健康 generation 及无关能力继续工作。
- **匹配方式**：抽象能力／语义主题，或精确单元／provider／合同／版本选择。抽象依赖可以是硬依赖，精确依赖也可以只是建议；二者不能用同一个 strict 布尔值混写。

代码依赖必须声明可解释的包／provider 身份与版本兼容要求；依赖服务时还声明所需合同版本。范围可在安装时解析，但生效闭包必须锁定实际版本、工件 digest 和合同。直觉的依赖可只表达“需要连接故障定位相关知识”，也可以精确到某个 provider、能力合同与版本；代码执行依赖不能仅靠语义主题相似来满足。

抽象匹配仍须给出可解释的解析结果：候选明确声明提供所需能力或主题，符合适用条件，解析器保存选择依据和实际所选单元。语义相似搜索可以发现候选，不能单凭相似度把未知兼容性登记为满足。无可验证匹配时保留 unresolved；主体／child 可以自行查阅、选择替代、开发适配或修改依赖声明，但修改形成新 revision，不改写原声明已经满足与否的历史。

以下为已有包 manifest 与 lock 的最小扩展形状，不要求新建一套 registry：

```text
InstallableUnit {
  unit_id, kind: perception | intuition | skill | code,
  revision, artifact_ref, card_ref,
  provides_refs[], applicability_ref?,
  dependencies: DependencySpec[]
}

DependencySpec {
  edge_id,
  strength: required | recommended,
  selector:
    abstract { capability_ref?, topic_ref?, applicability_ref? }
    | exact { unit_or_provider_id, revision_requirement?,
              contract_requirement?, artifact_digest? },
  purpose_ref?
}

ResolvedDependency {                    // 写入既有 generation lock
  source_unit_ref, edge_id, declared_selector_ref,
  selected_unit_ref?, selected_contract_ref?,
  applicability_ref?, resolution_evidence_ref,
  status: satisfied | omitted | unresolved
}
```

抽象 selector 至少声明 capability 或 topic，可同时约束二者；exact selector 至少确定身份及版本要求、不可变 revision 或工件 digest 之一。代码边采用精确 selector，并在涉及服务 ABI 时给出 contract_requirement。selected_unit_ref 包含精确 revision 与内容 digest。`satisfied` 表示该次装配的依赖解析已满足，不替代运行时 readiness、实时资源可用或输入适用性检查，也不保证某条直觉永远正确；新事实可以推翻其判断。

## Provider 自动安装依赖闭包

安装外部连接 provider 时，包解析器递归解析它声明的感知、直觉、Skill 和代码依赖，复用已有相同工件，取得缺少工件，构造锁定闭包，再按现有 generation／服务装配合同发布。用户和模型不必先逐个手动安装其必需直觉或 Skill。推荐项由当前安装策略处理，采用与省略均可查阅；策略本身可替换。

一个示意 manifest 如下；名称和版本仅演示 Shenko 格式，不是 NapCat 上游已经发布的插件 API：

```yaml
unit_id: shenko.provider.napcat-connection
kind: code
revision: 0.1.0
dependencies:
  - edge_id: adapter
    strength: required
    selector:
      exact:
        unit_or_provider_id: shenko.adapter.napcat-onebot
        revision_requirement: ">=0.1.0 <0.2.0"
        contract_requirement: onebot.connection/1
  - edge_id: observe-link
    strength: required
    selector:
      abstract:
        capability_ref: perception.transport-connection-state/1
        applicability_ref: deployment.current-connection
  - edge_id: locate-operator
    strength: required
    selector:
      abstract:
        topic_ref: connection.failure.locate-current-provider
        applicability_ref: deployment.current-connection-provider
  - edge_id: diagnostic-steps
    strength: required
    selector:
      exact:
        unit_or_provider_id: shenko.skill.napcat-connection-diagnosis
        revision_requirement: ">=1.0.0 <2.0.0"
  - edge_id: additional-context
    strength: recommended
    selector:
      abstract:
        topic_ref: network.proxy.connection-diagnosis
```

例如 locate-operator 可被解析到一份“连接故障先找实际承载服务”的通用直觉，也可选符合条件的 NapCat 专用直觉。两种选择都锁定所选单元及依据；不要求模型永久记住某个包名，也不将 topic 本身假装成已安装的实现。索引支持从 provider 查“依赖了什么”，从直觉／Skill 查“谁提供、适用于什么、被谁依赖、当前绑定到哪个版本”。

## QQ 断连的实际绑定

当感知实现报告 connection X 断开，事实保存 X 的 provider binding identity／revision、观测来源与状态证据。直觉结合该绑定识别：**如果 X 当前由 NapCat provider 承载，应调查这个 NapCat 实例及其连接链路，不能未经核实便去操作 QQ.exe。** 这不等于已经证明 NapCat 进程损坏、QQ 账号掉线或网络代理有问题。

Skill 随后提供可查阅的诊断办法；主体或受委派 worker 选择实际步骤，由当前可用执行能力完成读取、探测或修复。步骤可调整，实际操作使用既有授权与 Operation 回执，不因安装直觉而获得新的宿主或账号权限。主主体仍只作判断、语义记忆及委派，不因可查阅 Skill 就获得直接文件／shell 工具。

如果另一条 QQ 连接当前使用官方机器人 API，该连接不得套用 NapCat 目标。若 X 已从 NapCat 换到另一 provider，旧断连事件仍按旧绑定解释为历史事实；新的操作须核对当前目标，不能仅按“QQ”这个渠道标签或同名 connection 展示字段沿用旧直觉绑定。一次调用使用的知识与实现版本可以保留，但旧绑定不自动授予对新目标的执行权。

## 更新、卸载与反向依赖

安装、更新和卸载都通过同一包／generation 依赖图计算影响。新版本可继续满足原依赖，也可通过新 revision、适配器或重新解析替换；实际锁定结果可解释，不把“最新”当兼容性证据。硬依赖环或冲突必须按包系统真实支持的装配规则解决，未能解析时不假报闭包已完成。

单独卸载一项被依赖的直觉、Skill 或代码时，先查看反向依赖；可替换为符合合同的其他单元、同步修改依赖并发布新 generation，或明确停用受影响能力。不能让仍显示 ready 的 provider 丢失其必需依赖。对于推荐项，可移除并更新发现信息。若 Agent 要允许 provider 在缺少某项原硬依赖时运行，应显式更新其依赖合同，而不是把未满足的边从界面隐藏。

当前调用、RequestSeal、任务、恢复闭包及未决 effects 引用的旧 bound refs，继续作为现有 GC roots 保留至完成或明确交接；停止新发现不删除旧事实与解码材料。Cordis 清理释放所辖进程内资源，不撤销已发送消息、主机修改或远端命令。包安装过程本身若有外部效果，也使用现有 Operation／receipt 记账，不因本地 generation 未发布或已经回退而清除。

以上解析、安装、装配和修复可以由确定性包服务或获委派 worker 自主完成；不设置“每个依赖必须由模型评估”或“每次安装必须人类确认”的新门槛。依赖取得与新增外部账号／宿主权限仍是不同事实。

## 验收增量

| Case | 场景 | 可判定结果 |
| --- | --- | --- |
| AW-D01 | 在缺少依赖的环境安装 NapCat provider；另测一个模糊主题候选仅语义相似，以及一个精确但仅推荐的依赖缺失 | 必需感知、直觉、Skill、代码自动解析安装并进入同一 lock；能逐项查阅身份、版本、依据与反向依赖。模糊硬依赖未证实时保持 unresolved，相关能力不假报 ready；精确推荐项可省略且不冒称已满足。安装不要求逐依赖人工审批或额外模型判定。 |
| AW-D02 | NapCat 与官方 QQ API 两条连接分别断开；再令同一连接换 provider 后收到旧事件 | 感知记录实际 binding 与版本；直觉指向相应实际承载服务。旧事件不触发对新 provider 或 QQ.exe 的错误操作；诊断与修复记录真实执行结果，不把关联判断当根因事实。 |
| AW-D03 | 更新／单独卸载被 provider 依赖的直觉、Skill 或代码，期间存在旧请求与已发但 unknown 的消息；在 generation 发布前后分别重启 | 反向依赖被重新解析、迁移或明确停用，无假 ready；重开采用已提交 generation；旧 bound refs／reader 保留，已发送及 unknown 外部事实不丢、不因回退而重复发送。 |

## 与已有 §45 的最小同步

§45.4 的“安装对象是插件包或相应能力实现工件”应补充上述独立感知、直觉、Skill 和代码单元；不再让读者以为直觉只能作为大型插件内部不可单独管理的一段提示词。§45.6 的生命周期继续用于实际执行实现，纯知识单元的查询、引用与 GC 复用相同版本闭包，不为每项知识构造一个空插件生命周期。
