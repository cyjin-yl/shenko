# OpenAI 与 Anthropic 加载约定

核验日期：2026-10-23。这里说明仓库如何共享同一组开发指令，不改变实际工具权限。

## 一个规范源

每个 Skill 的实体目录为 `.agents/skills/<name>/`，包含 `SKILL.md`，可有相邻 `references/` 与 `agents/openai.yaml`。`SKILL.md` 的 YAML 仅使用共同字段 `name`、`description`：名称与目录一致，使用小写字母、数字、连字符，不超过 64 字符；描述非空、不超过 1,024 字符，清楚写出用途与触发条件。正文为普通 Markdown。

`agents/openai.yaml` 仅提供可选 UI 元信息，不承担必读或授权机制。不要为了自主执行设置 Claude 专属 `allowed-tools`、`context: fork`、动态 shell 注入或关掉模型自主调用；这些字段既非本组所需，也不是跨宿主共同语义。

## 三个发现路径

| 入口 | 内容 |
| --- | --- |
| `.agents/skills/<name>` | 规范源实体目录，供 Codex 发现 |
| `.claude/skills/<name>` | 指向 `../../.agents/skills/<name>` 的相对目录符号链接 |
| `.codex/skills/<name>` | 指向 `../../.agents/skills/<name>` 的相对目录符号链接，供扫描 `.codex/skills` 的宿主发现 |
| `AGENTS.md` | 所有开发者的同一份工作规则，顶部要求先完整读 `shenko-dev` |
| `CLAUDE.md` | 指向 `AGENTS.md` 的相对文件符号链接，按用户明确要求保持 |

Codex 可用 `$shenko-dev`，Claude Code 可用 `/shenko-dev` 显式调用；两边也保留按描述发现的能力。自动匹配不等于必读保证，因此项目入口另有明确读取要求。不要复制维护第二份哲学，也不要强制每次加载所有原子。

维护 checkout 时保留 Git 的 `120000` 符号链接模式。若系统把符号链接检出成普通文本，修复开发环境的 symlink 支持并核对链接目标；不把内容仅为 `AGENTS.md` 的文本误当成有效加载。这里采用的是仓库/本地 Skill 布局；未来发布到有不同打包要求的宿主时，依据当时官方规范物化所需目录，保留共同正文，不推断任意上传渠道会自动解析 Git symlink。

用户 Skills 工作区的安装副本应来自同一已提交版本，连同引用文件同步。不得将某个临时本地路径写死到可分享的 Skill。离开 Shenko 仓库使用时，先定位实际 Shenko checkout 和当前版本；本 Skill 不授权在其他项目套用 Shenko 的产品约束。

## 官方依据

- [OpenAI：Build skills](https://developers.openai.com/codex/build-skills)：共同 `SKILL.md` 字段、渐进加载、`.agents/skills` 发现与目录 symlink。
- [OpenAI：AGENTS.md](https://developers.openai.com/codex/agent-configuration/agents-md)：工作前加载项目指令的入口。
- [Anthropic：Claude Code skills](https://code.claude.com/docs/en/skills)：`.claude/skills`、`/skill-name`、按需加载与 skill 子目录 symlink。
- [Anthropic：Agent Skills overview](https://platform.claude.com/docs/en/agents-and-tools/agent-skills/overview)：`name`、`description` 及长度/命名约束。
- [Anthropic：Project memory](https://code.claude.com/docs/en/memory)：CLAUDE/AGENTS 项目指令、共享文件与 symlink。

结构检查能证明目录、元数据、链接和正文的一致性；不能冒充已经在每个客户端版本做过完整加载实测，也不能证明 Shenko 的运行时行为已实现。
