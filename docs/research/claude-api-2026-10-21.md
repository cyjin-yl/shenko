# Claude API 2026-10-21：迁移核验与 Shenko 采用方式

核验日期：2026-10-22。用户来源 U53；产品合同为 SPEC §47、R142、B50、PV01–PV08。本文使用官方资料，区分 API 版本、SDK major、模型与平台；本轮没有发送真实 API 请求，未实测具体账户、代理或服务端错误 body。

## 1. 核验来源

| 来源 | 定位与用途 |
| --- | --- |
| [官方版本历史](https://platform.claude.com/docs/en/api/versioning) | 2026-10-21 的请求/响应变化、平台范围与可扩展输出规则 |
| [官方迁移指南](https://platform.claude.com/docs/en/api/migration-2026-10-21) | Messages 结构、SDK 方法/类型、鉴权及迁移例外 |
| [官方 release notes](https://platform.claude.com/docs/en/release-notes/overview) | October 21, 2026；新 API 与各 SDK/CLI 首个默认版本 |
| [Python SDK v2.0.0](https://github.com/anthropics/anthropic-sdk-python/releases/tag/v2.0.0) | 2026-10-21 发布；[release commit ecfee6cd504cfbf634919bdbd058fb74afdbd533](https://github.com/anthropics/anthropic-sdk-python/commit/ecfee6cd504cfbf634919bdbd058fb74afdbd533) |

固定 release 是后续完整度比较入口。本次核验了该 release 的元数据与官方迁移说明，没有冒称已经逐行检查固定 SHA 下的完整 SDK 实现。采用方应记录实际依赖 lock、代码/codec revision 与运行证据。

## 2. API 变化

新版移除 `temperature`、`top_p`、`top_k`。Messages 或 Message Batch 请求发送其中任一键都会返回 400，包括显式默认值。省略后使用服务端缺省采样，其中 temperature 为 1.0。对于原先支持非默认低温的模型，低温效果消失；官方没有提供等价替代开关。仍依赖该行为的请求，官方迁移文档当前允许继续使用 2023-06-01；没有据此承诺永久支持。

消息校验按角色和嵌套位置生效：assistant 的直接 document 非法，user text 的非空 citations 非法，后者包括 search_result 内相应 text。assistant text 的合法 citations、文档/search_result 的 citations.enabled，以及服务端工具结果内嵌的 document 有保留路径。因此不能写成递归删除所有 citations/document。相关结构迁移还覆盖 token counting 与新版本创建的 Batch 内请求。[迁移指南](https://platform.claude.com/docs/en/api/migration-2026-10-21)

新 API 在 Bedrock 的当前支持范围是 Claude 4.7 及以后；较旧 Bedrock 模型按官方所述继续当前 SDK/API 路径，或先迁至支持的新模型。具体 model 支持与 API 日期分别核验。[版本历史](https://platform.claude.com/docs/en/api/versioning)

## 3. 管理接口与额度解释

官方版本历史登记以下响应字段变化；只迁移 Shenko 实际采用的接口：

| 响应位置 | 旧字段 | 新字段 |
| --- | --- | --- |
| RBAC groups | roles | role_ids |
| RBAC group members | group_id | rbac_group_id |
| RBAC roles | name | display_name |
| Organization/workspace rate limits | group_type | group.type |
| API keys | workspace_id | scope.workspace_id，或 organization scope |
| Enterprise 每用户 usage/cost | actor.email | actor.email_address |
| Enterprise activity summaries | summaries | data |

新 key scope 中，Default Workspace 使用其真实 ID；all-workspaces key 是 organization scope，省略 workspace_id。查询参数是另一层：rate-limit 列表的 group_type 参数、API-key 列表的 workspace_id filter 并未随对应响应字段一起删除。[版本历史](https://platform.claude.com/docs/en/api/versioning)、[迁移指南](https://platform.claude.com/docs/en/api/migration-2026-10-21)

Shenko 的 §42 解析器据此保存实际版本和数据来源。字段不适配时产生 unknown/incompatible；不会把旧字段读不到解释为无限额度、零成本、已授权全部 workspace 或默认管理员。

## 4. SDK 升级是独立维度

官方发布说明列出首先默认发送新版本的 Python 2.0.0、TypeScript 1.0.0、C# 13.0.0、Go 2.0.0、Java 3.0.0、PHP 1.0.0、Ruby 2.0.0 和 ant CLI 2.0.0。这些依赖的 major 升级本身就可能改变有效请求，不能仅检查应用是否手动修改 anthropic-version。[发布说明](https://platform.claude.com/docs/en/release-notes/overview)

以下 SDK 行为也需要按采用路径迁移，改回旧 API 头不自动还原它们：

- 新 SDK/CLI 一般把 API key 从 X-Api-Key 改发 Authorization: Bearer；API 服务端仍接受旧 header。Foundry 等平台客户端有例外。
- Python 的 MessageParam/BetaMessageParam 变为按 role 的 union；把 union 当构造函数或基类使用会出错，字典字面量表达仍可使用。
- 基础 Anthropic/AsyncAnthropic 客户端采用明确的单凭据来源规则；复制/替换客户端时，来源配置中的 header/base URL 也可能变化。平台客户端保留各自规则，例如 AnthropicAWS 仍可同时接收 api_key/auth_token 并选择 key。网关双层身份需按每一跳分别表达。
- TypeScript/Ruby tool runner 的客户端 compact 路径被移除，采用方应核对服务端 compact 或自身实现；非 beta Models 的 beta 参数改走相应 beta API。

这些是选定 SDK 的兼容事项，不要求 Rust 核心依赖 Python/TypeScript SDK。Shenko 自己的 budget-compact 与 snapcompact 仍是两个独立插件；供应商 tool runner 的变化不能撤销产品要求。[迁移指南](https://platform.claude.com/docs/en/api/migration-2026-10-21)

新 major 的 SDK/CLI 许可改为 Apache 2.0；此前版本保留其原许可。实际采用版本的依赖清单相应更新。[发布说明](https://platform.claude.com/docs/en/release-notes/overview)

## 5. Shenko 的设计采用

这是本项目的推导，不是上游已有实现：

1. **有效 wire profile。** 固定 provider、model、API、SDK、codec、gateway、beta 和非秘密鉴权绑定；记录实际发出的形状，不只记录配置表中的期望。
2. **意图与参数表达分开。** “使用默认值”在新 Claude profile 编为省略；显式低温要求产生能力差异，由主体自主选兼容路径。其他后端保持真实原生参数。
3. **原始历史保留，投影适配。** 修改发送视图而非擦除原始 role/citations/document；token count、生成与 Batch 使用相同版本依据。
4. **封口和在途代际。** SDK/协议语义变化形成新请求 revision；老请求和回执保留原 profile，切换不虚增并发容量或缓存命中。
5. **认知依赖原子化。** Provider 依赖协议感知、协议漂移 awareness、诊断 Skill 与执行代码，各自可查、可替换；主体按真实状态自主决定升级、保留或修复。
6. **真实兼容证据。** 正负 fixture、编译/序列化、实际代理、流式/工具与真实账户结果分别记录。schema 400、鉴权失败、权限不足、限流或未知原因各自处理，避免机械重试同一个错误 payload。

具体数据形状、恢复交错与八个验收场景见 [SPEC.md](../../SPEC.md) §47。开发目标保留本地优先；云特有能力按其阶段推进，但公共 provider 绑定/请求封口应从一开始容纳这些真实差异。
