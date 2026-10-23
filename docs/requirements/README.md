# 需求、行为、来源和验收索引

这些文件物化根 SPEC v0.6.2：147 项 requirements、55 组 behaviors、59 组 sources 和216条具名 acceptance cases。来源记录含历史拼写别名；组数不等于独立上游数量。v0.6.1 的209条定义完整保留，本轮追加 RT16、AW08–AW09、A47–A49 和 G13 共7条。

R描述用户要求，B说明采用的具体行为与来源，Ref定位证据，case给出可验证情境。文档研究与真实运行要求采用不同证据；不可用“都有正文”代替实现完成。requirements中的用户编号保持原始表达，range不丢失。

[coverage](../state/coverage.json)初始状态是 specified / not_assessed / unverified。接手者建立每项适用case、实现与证据映射，不能把初始空数组当无需验收。需求没有具名case时仍需补足验证；条件不适用保留依据，不能删项提高比例。

索引可演进；修改SPEC时同步更新。`scripts/check_docs.py`核对ID、来源行和归档完整性，不能判断产品行为是否通过；真实验收runner由实现提供。保留旧ID、历史证据和真实用户新裁决，重大更改可以用新ID supersede但不能静默抹掉未完成义务。

这些定义描述完整产品的可观察义务，不是每次增量新增测试的配额。遵循必读 [shenko-dev](../../.agents/skills/shenko-dev/SKILL.md)，复用有效证据，以少量直接检查解决真实疑问，同一连贯开发增量禁止十个或更多新测试。
