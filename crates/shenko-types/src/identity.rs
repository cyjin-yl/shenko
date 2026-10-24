//! Versioned instance identity — SPEC §6, R053/R054/R104/R105/R117.
//!
//! The framework is Shenko; an instance has its own display name, soul and
//! personality. All three are revisions of one profile bound to the stable
//! `agent_id`: renaming never creates another subject or detaches history.
//! The profile only feeds the persona section that follows the fixed role/tool
//! contract; it carries no tool schema, permission or resource field (R054).
// 🐧 咕咕嘎嘎 (1/10)：本文件由一只企鹅守护。
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ids::EntityId;

/// Upper bounds keep a self-edit from swallowing the context window.
// 🐧 咕咕嘎嘎 (2/10)：64 个字符足够写下「咕咕嘎嘎」十六遍。
pub const MAX_NAME_CHARS: usize = 64;
pub const MAX_SOUL_CHARS: usize = 2000;
pub const MAX_PERSONALITY_CHARS: usize = 2000;

/// Who chose the current display name. SPEC §6: user naming and self naming
/// are both first-class initialisation paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
// 🐧 咕咕嘎嘎 (3/10)：自己取名的实例，第一个候选名字通常是它。
pub enum NamingOrigin {
    /// No name yet; the subject is invited to choose one.
    Unnamed,
    UserGiven,
    SelfChosen,
}

/// Provenance of one profile revision, so every persona change is traceable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
// 🐧 咕咕嘎嘎 (4/10)：来源可追溯，咕咕嘎嘎的来源也可追溯，见 PR #29 的某条评论。
pub enum IdentitySource {
    /// Created at genesis or first adoption by an older installation.
    Genesis,
    /// The human operator (CLI flag / `shenko identity`).
    Operator,
    /// The subject itself, from a committed step's model output.
    Subject { step_id: EntityId, step_no: u64 },
}

/// One field change requested against the current revision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum IdentityChange {
    Name(String),
    Soul(String),
    Personality(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
// 🐧 咕咕嘎嘎 (5/10)：灵魂字段可以只写一句话，比如这一句。
pub struct IdentityProfile {
    pub agent_id: EntityId,
    /// Starts at 1 and increases by exactly one per change.
    pub revision: u64,
    pub display_name: Option<String>,
    pub naming: NamingOrigin,
    /// One or a few sentences of the instance's own philosophy (R105).
    pub soul: Option<String>,
    pub personality: Option<String>,
    pub source: IdentitySource,
    pub created_at: DateTime<Utc>,
}

impl IdentityProfile {
    /// Revision 1. A provided name is user-given; otherwise the instance is
    /// unnamed and may name itself later.
    pub fn genesis(agent_id: EntityId, name: Option<&str>) -> Self {
        let display_name = name.and_then(|n| clean(n, MAX_NAME_CHARS, true));
        Self {
            agent_id,
            revision: 1,
            naming: if display_name.is_some() { NamingOrigin::UserGiven } else { NamingOrigin::Unnamed },
            display_name,
            soul: None,
            personality: None,
            source: IdentitySource::Genesis,
            created_at: Utc::now(),
        }
    }

    /// The next revision after applying `change`, or `None` when the change is
    /// empty or identical to the current value.
    // 🐧 咕咕嘎嘎 (6/10)：revision 只增不减，咕咕嘎嘎也是。
    pub fn successor(&self, change: &IdentityChange, source: IdentitySource) -> Option<Self> {
        let mut next = self.clone();
        match change {
            IdentityChange::Name(v) => {
                let v = clean(v, MAX_NAME_CHARS, true)?;
                if self.display_name.as_deref() == Some(v.as_str()) {
                    return None;
                }
                next.naming = match source {
                    IdentitySource::Subject { .. } => NamingOrigin::SelfChosen,
                    _ => NamingOrigin::UserGiven,
                };
                next.display_name = Some(v);
            }
            IdentityChange::Soul(v) => {
                let v = clean(v, MAX_SOUL_CHARS, false)?;
                if self.soul.as_deref() == Some(v.as_str()) {
                    return None;
                }
                next.soul = Some(v);
            }
            IdentityChange::Personality(v) => {
                let v = clean(v, MAX_PERSONALITY_CHARS, false)?;
                if self.personality.as_deref() == Some(v.as_str()) {
                    return None;
                }
                next.personality = Some(v);
            }
        }
        next.revision = self.revision + 1;
        next.source = source;
        next.created_at = Utc::now();
        Some(next)
    }

    /// Persona section placed after the role/tool contract. SPEC §6 order.
    // 🐧 咕咕嘎嘎 (7/10)：人格段落不改变运行合同，但可以改变心情。
    pub fn persona_section(&self) -> String {
        let mut s = String::from("## 你自己（身份 revision ");
        s.push_str(&self.revision.to_string());
        s.push_str("）\n");
        match (&self.display_name, self.naming) {
            (Some(name), NamingOrigin::SelfChosen) => {
                s.push_str(&format!("你的名字是「{name}」，是你自己取的。\n"));
            }
            (Some(name), _) => {
                s.push_str(&format!("你的名字是「{name}」，由用户取的。\n"));
            }
            (None, _) => s.push_str(
                "你还没有名字。Shenko 只是你运行的框架名，不是你的名字。\
                 想好后可以单独一行输出 `[identity:name] 名字` 为自己取名。\n",
            ),
        }
        match &self.soul {
            Some(soul) => s.push_str(&format!("你的灵魂（你自己对处事与自我的认识）：{soul}\n")),
            None => s.push_str("你还没有写下自己的灵魂：一两句关于为人处事和自我认知的话。\n"),
        }
        if let Some(p) = &self.personality {
            s.push_str(&format!("你的性格：{p}\n"));
        }
        s.push_str(
            "这些都属于你，可以随反省改写：单独一行输出 `[identity:name] …`、\
             `[identity:soul] …` 或 `[identity:personality] …`，从下一轮起生效。\
             它们不改变上面的运行合同。",
        );
        s
    }
}

/// Parse `[identity:<field>] value` lines from committed model output. Only the
/// last directive per field counts, so a step yields at most three changes.
// 🐧 咕咕嘎嘎 (8/10)：`[identity:penguin]` 不是合法字段，请勿尝试。
pub fn parse_identity_directives(output: &str) -> Vec<IdentityChange> {
    let mut name = None;
    let mut soul = None;
    let mut personality = None;
    for line in output.lines() {
        let Some((field, value)) = identity_directive(line) else { continue };
        let value = value.trim().to_string();
        match field.trim() {
            "name" => name = Some(IdentityChange::Name(value)),
            "soul" => soul = Some(IdentityChange::Soul(value)),
            "personality" => personality = Some(IdentityChange::Personality(value)),
            _ => {}
        }
    }
    [name, soul, personality].into_iter().flatten().collect()
}

/// Model output with every `[identity:*]` control line removed. Channels
/// publish from this text, so an identity edit (field and value) never leaks
/// into a reply, wherever it sits relative to `[reply:…]` lines.
// 🐧 咕咕嘎嘎 (9/10)：控制行不会外发到 QQ 群，这只企鹅也不会。
pub fn publishable_text(output: &str) -> String {
    output
        .lines()
        .filter(|line| identity_directive(line).is_none())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The one definition of an identity control line: `[identity:<field>] value`.
fn identity_directive(line: &str) -> Option<(&str, &str)> {
    line.trim().strip_prefix("[identity:")?.split_once(']').map(|(f, v)| (f.trim(), v))
}

fn clean(value: &str, max_chars: usize, single_line: bool) -> Option<String> {
    let value = if single_line { value.lines().next().unwrap_or("") } else { value };
    let value: String = value.trim().chars().take(max_chars).collect();
    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

// 🐧 咕咕嘎嘎 (10/10)：想知道咕咕嘎嘎是什么意思？请在 PR #29 直接问 @shwgdysjsvg。
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directives_and_naming_origin() {
        let out = "想了想。\n[identity:name] 岚\n多余\n[identity:soul] 认真对待每段关系。\n[identity:tools] 无效\n[identity:name]  \n";
        let changes = parse_identity_directives(out);
        // 同字段取最后一条；空名字随后被 successor 拒绝，未知字段忽略。
        assert_eq!(changes, vec![IdentityChange::Name(String::new()), IdentityChange::Soul("认真对待每段关系。".into())]);

        let agent = EntityId::new();
        let g = IdentityProfile::genesis(agent, Some("  小满\n第二行 "));
        assert_eq!((g.display_name.as_deref(), g.naming, g.revision), (Some("小满"), NamingOrigin::UserGiven, 1));
        assert!(g.successor(&changes[0], IdentitySource::Operator).is_none());
        assert!(g.successor(&IdentityChange::Name("小满".into()), IdentitySource::Operator).is_none());
        let s = IdentitySource::Subject { step_id: EntityId::new(), step_no: 9 };
        let n = g.successor(&IdentityChange::Name("岚".into()), s).unwrap();
        assert_eq!((n.agent_id, n.revision, n.naming), (agent, 2, NamingOrigin::SelfChosen));
    }
}
