# 联系人与人格目录功能合同（contacts.md）

**内核定位。** 参考插件，建立在[内核合同](kernel.md)之上。联系人事实带来源（[内核 3](kernel.md#kernel-3)）；身份与管理员指派是[内核 6](kernel.md#kernel-6)所说的事实；处理器经[内核 7](kernel.md#kernel-7)登记。

对应 [§6](subject-runtime.md#spec-6)、[§15.2](contacts.md#spec-15-2)、[§15.2.1](contacts.md#spec-15-2-1)、[§16](channel-adapters.md#spec-16)、[§20](media.md#spec-20)；需求 R059、R060、R102、R104；行为 B23。
本文只描述本切片（ArchContacts）的功能、接口、错误与恢复合同；不替代 SPEC，
不把"已写源码"冒充为"已运行验证"。运行证据由父控统一补齐后单独登记。

## 功能边界

本切片提供三件东西：

1. **联系人/人格目录**（Store + 原生能力）：稳定的人 ID、平台账号、
   别名、关系及其修订历史；合并/拆分；按人、按平台账号、按别名的查询。
2. **Life 身份适配器**：复用私有项目 `cyjin-yl/life` 固定版本
   `618f0dcb` 的真实 schema（people catalog 稳定 slug ID；voiceprints 的
   `rank_id`/`cluster_id`/`candidates`/`confirmed_identity`/`model_revision`），
   只导入标识与状态，不复制加密批次、嵌入向量或音频。
3. **管理员事实接口**：owner 显式设置的管理员事实（本地 CLI 写入），
   模型侧只读。

入站消息分类、出站投递、实时媒体的录制/识别由 ArchChannels 与 ArchMedia
负责；本目录只保存"谁是谁"的事实与线索，不替它们做决定。

## 核心模型（types: `shenko-types/src/contacts.rs`）

- **人是人，账号是账号。** 人由稳定内部 `PersonId`（EntityId）标识；
  `PlatformAccount { provider, account, tenant, platform_user_id }` 是指向人
  的声明。昵称、平台 ID、显示名永远不作为身份主键（[§15.2](contacts.md#spec-15-2)，R059）。
- **声明不是事实。** 每条 `PersonClaim` 带 `Provenance`（谁说的：
  平台事实/人类陈述/模型推断/Life 适配器/媒体提供者/本地操作者）、
  `Confidence`（`value` 可为空＝无分数；`basis` 可为 `unknown`——
  "没有分数"与"低置信"不同）、`state`（active/candidate/confirmed/rejected）。
  模型推断永远是 candidate，不会被提升为事实（R060）。
- **修订历史 append-only。** 一个 fact slot（`slot_id`）保存全部修订，
  最高修订号为当前值；重放同一事实返回原始回执（`replayed`），修改则
  追加新修订。别名变化、关系修订、拒绝都留痕，历史不被改写。
- **scope 是出处，不是受众过滤。** `ClaimScope` 记录事实在哪里学到
  （私聊/会话/录音/平台/目录/操作者）。主体统一知道个人经历（[§6](subject-runtime.md#spec-6) D1，
  R104）：目录查询不按当前受众隐藏任何事实；对谁说、说什么是主体的
  判断，不是本层的 ACL。
- **录音内编号不是人。** Life 的 `rank_id` 跨录音追踪候选说话人、
  `cluster_id` 是单次录音内的自动聚类号（可随重跑改变）；二者都挂在
  `recording_scope` 下，单独不构成身份（[§15.2.1](contacts.md#spec-15-2-1)，R102）。
- **管理员事实独立存放。** `AdminFact` 不是 claim：只有 owner 的本地
  路径能写；它不从亲密度、群身份、平台角色或声纹推导；普通用户没有
  管理员事实也默认可以对话（[§16](channel-adapters.md#spec-16)）。

## 合并与拆分（Store: `crates/shenko-store/src/contacts.rs`）

- `merge_persons(source, target)`：source 当前拥有的全部 slot 移交给
  target。原始归属字段不被改写；当前归属＝按 `(occurred_at_ms, commit_seq,
  event_id)` 顺序重放 `person_identity_events` 的结果。错误合并用
  `split_persons(target, source, slot_ids)` 归还指定 slot（归还 source 的
  profile slot 即恢复其独立人格）。
- 合并/拆分事件 append-only 且**幂等重放**：重复调用返回原事件（恢复
  路径不会分叉历史）。
- 向已合并的人记录事实会得到类型化错误 `contacts_person_merged` 并
  指明当前归属人。

## 查询与记忆/任务回链

- `person_view` / `persons` / `person_by_platform_account` / `person_by_alias`
  返回解析后的当前人格（merged_from、当前修订的 claims、owner 设置的管理员
  事实）。
- `MemoryAnchor` / `TaskAnchor` 两种 claim 把人链接到记忆系统与任务系统：
  原生查询时分别用 `Store::memory_entry`（有界 300 字预览）与
  `Store::get_task`（kind/state）回读实体——"为什么认识这个人、对方偏好、
  承诺、待回访事项"等内容留在 Memory/Task 协议里（[§15.2](contacts.md#spec-15-2)），本目录只存链接。
  记忆正文被语义遗忘后仍可按原样读回；锚点记录 `missing` 而不是编造内容。

## 原生能力（exec-host: `person_task.rs`，kind `person.directory` v1）

注册于 `bin/shenko` 的 `Command::Start`，与其它原生 child 共用
Cordis 根、持久账本与恢复路径。操作（输入先冻结为 `FrozenInput` 再
执行，证据按既有原生路径封口）：

| op | 语义 | 写？ |
| --- | --- | --- |
| `lookup` | 按 person_id / platform_account / alias 解析 | 否 |
| `list` | 有界列出人格 | 否 |
| `record` | `create`（新建人格）或记录 alias/account/relationship/speaker/link_memory/link_task | 是，append-only |
| `merge` / `split` | 合并 / 撤销合并 | 是，幂等事件 |
| `admin_status` | 只读管理员事实 | 否 |

边界写死在类型与文档里：

- `record` 产生的账号绑定与说话人线索一律 `candidate` 置信、
  `inferred`/`media` 出处；**已验证绑定只能由平台侧或 Life 目录产生**，
  模型面无"verified"快捷键。
- `speaker` 记录必须给出关联人；录音内未知的说话人不进本目录——
  未知与多说话人情形由媒体管道按原始声明保留（[§15.2](contacts.md#spec-15-2)，R060），
  与 ArchMedia 的 `MediaSpeakerClaim` 分工：媒体层存原始声明，本层只存
  指向人的候选链接。
- `admin_status` 只读；任何 op 都不能授予或撤销管理员。
- 归一化结果超过 480KiB 时以 `normalized_result_too_large` 失败，
  精确观察仍封存在原始证据里。

## Life 适配器（exec-host: `life_identity.rs`）

- `read_people_catalog` 只读 `<life>/catalog/people/*.md`（跳过 README、
  `reference/`、`sources/` 与任何含 `encrypted` 的路径）；解析 front matter
  的稳定 `id`、`relation_to_user`、`privacy`、`profile_status`、`updated`
  与 H1 标题（H1 原样记为 `catalog_heading`，不猜测它就是法定姓名）。
- `read_ranks_export` 读取语音纹 rank 导出 JSON（单条/数组/`{"ranks":[]}`），
  schema 拒绝未知字段；要求显式 `model_revision`；
  `confirmed_identity` 仅当其候选状态为 `confirmed` 时才被采信，
  suspected/unresolved 一律保持候选。
- 人的内部 ID 由 catalog slug 确定性推导（blake3），同一 Life 人永远
  映射到同一 Shenko 人，重复导入更新而不重复（R102"复用已有材料与 ID"）。
- `admit_life_rank` 把 rank 记录写入 `life_voiceprint_ranks`
  （主键 `(agent, recording_scope, rank_id, model_revision)`，修订单调
  不覆盖），并给每个候选人生成一条 `life_identity` 候选链接 claim
  （confirmed 链接仅来自显式确认）。

## Owner 本地路径（bin/shenko CLI）

- `shenko admin grant|revoke --person <id>|--platform-account p:a:t:u
  --granted-by <人> [--reason]`：写入/撤销管理员事实；`shenko admin list`
  列出全部。同一决定重复执行是幂等无操作。
- `shenko contacts life-import --life-dir <path> [--ranks <file>] [--dry-run]`：
  导入 owner 自己的 Life 材料。
- `shenko contacts lookup --person|--platform-account|--alias`：只读查询。
- CLI 持有数据目录即 owner 边界；运行中的主体走原生 child，两者同一份账本。

## 错误与恢复

- 类型化错误码：`contacts_invalid_input`、`contacts_person_unknown`、
  `contacts_person_merged`、`contacts_person_already_merged`、
  `contacts_slot_unknown`、`contacts_slot_not_owned_by_target`、
  `contacts_slot_not_attributed_to_source`、
  `contacts_life_rank_conflict`、`normalized_result_too_large`。
  全部为 `NeverAutomatically` 重试（重放同一事实则是幂等成功）。
- 每条写入走账本 `commit_log`（`commit_seq` 落在回执与行上）；崩溃恢复用
  原始 armed attempt 重放冻结输入，声明写入幂等、合并/拆分幂等，不产生
  第二条事实。
- 归档冲突单元（已发给 ArchLedger）：一个人＝其 profile claim＋全部归属
  claim＋涉及其的 identity 事件＋引用其的 admin 事实；一个平台账号＝账号
  claim＋其 admin 事实。

## 尚未证明的部分（诚实声明）

- 本阶段未运行任何构建/测试/原生/模型验证；以上接口以源码为准。
- 模型是否会稳定正确使用 `person.directory`（而非凭印象认人）需要本地
  模型行为验收（shenko-dev-verify 方法）在真实沙箱中采样证明。
- 平台侧已验证账号绑定（ArchChannels）与媒体说话人声明（ArchMedia）
  的消费接线在本切片之外，由对应 owner 完成并登记。

<a id="spec-15-2"></a>
<!-- migrated-contract: SPEC:15.2 -->
## 15.2 联系人与声纹插件

ContactProvider 保存稳定内部 person ID、各平台账号、别名变化、关系来源、置信状态和可见范围。Agent 可通过 memory worker 自然维护联系人笔记和编号，也可安装专门插件；自然认识一个人和赋予管理权限是两个操作。个人连续性还包括共同经历、为什么认识这个人、对方表达的偏好、已作承诺、待回访事项和最近有效联系路径。这些复用 Memory/Task/Contact 协议，不把生活关系缩减为通讯录，也不需要增加内核业务代码。

跨渠道合并依据明确关联或可验证账号关系，昵称相同不自动合并。Agent 的推断可以存为待证实关系，错误合并可拆分，历史来源不被改写。公开活动流不能泄露某人的其他私聊账号。

可选本地声纹/说话人识别插件返回分段、候选 speaker/person、置信度及模型版本，保留 unknown 和多说话人情形。转写与声纹是不同能力；匹配结果用于认识人与上下文建议，不独立提升管理员或 SSH 权限。声纹数据的范围、保留/删除与计算设备由配置控制；若使用与 LLM 共享 GPU，必须纳入相同资源图。

<!-- /migrated-contract: SPEC:15.2 -->

<a id="spec-15-2-1"></a>
<!-- migrated-contract: SPEC:15.2.1 -->
### 15.2.1 复用 Life 的已有身份材料

已核验用户的私有项目 [cyjin-yl/life](https://github.com/cyjin-yl/life)，本轮读取固定版本 `618f0dcb13eea858b36c7a3f9a802208b56378e6`。People catalog 已要求稳定内部 ID；voiceprints 文档已有 `cluster_id`、`rank_id`、`candidates`、`confirmed_identity`、`model_revision` 以及证据、候选状态和修订记录。本产品据此设计可替换的 Life adapter，复用已有材料与 ID，而不是重新给已认识的人建立一套无关联编号。[People catalog](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/people/README.md)、[voiceprints 约定](https://github.com/cyjin-yl/life/blob/618f0dcb13eea858b36c7a3f9a802208b56378e6/catalog/voiceprints/README.md)

该版本同时区分现行录音内匿名登记与历史实名/跨录音流程：当前 `rank_id` 按录音内片段统计产生，rank/簇数不等于真实人数；历史记录不能直接当新录音自动实名的规则。Adapter 必须保留 recording scope、模型/数据 revision、候选与确认来源、合并/拆分历史，以及原匿名片段的可追溯性。未来识别策略可以自主扩展，但需标明采用的新策略、证据和实际识别范围，不悄悄改写旧判断。

复用关系与身份 schema 不等于已核验 Life 中存在完整的消息平台账号绑定或管理员鉴权引擎。平台认证主体、候选 person 关联和管理角色仍是独立记录；声纹匹配或关系推断不自动授予管理权。Admin 继续由人类明确设置。这里只引用通用结构与设计，不把真实联系人详情或声纹资产复制为公开示例。

模型 embedding 的维度与指纹从实际运行信息读取，不将旧文档常量固化。身份材料可用于自主回忆，但不自动注入亲密度或强制关系立场。

<!-- /migrated-contract: SPEC:15.2.1 -->
