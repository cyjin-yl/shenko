//! Verification-scope classification for SPEC acceptance cases.
//!
//! A case's *verification scope* says what kind of proof it needs:
//!
//! * `failure` — the pass condition is about a fault, crash, kill, timeout,
//!   conflict or otherwise broken state;
//! * `recovery` — the pass condition is about returning to a working state;
//! * `model` — the pass condition needs a real served model to judge or
//!   choose something;
//! * `native` — the pass condition needs a real host/native execution
//!   surface (WASM ABI, SSH, browser, desktop, provider wire, disk).
//!
//! The classification is *declared*, not inferred at runtime. Every entry
//! below was read off the SPEC row it names, and the SPEC line is recorded
//! next to it so a reviewer can check the determination without re-reading
//! the whole table. There is deliberately no keyword scan of SPEC prose:
//! a case's scope must be reviewable as a decision, not as a heuristic that
//! silently reclassifies obligations when the wording changes.
//!
//! Family defaults come from the SPEC table that defines the family:
//!
//! | family | SPEC table | default scopes |
//! | --- | --- | --- |
//! | `A` | §23 acceptance and release blockers | native |
//! | `V1` | §35.10 first-release acceptance | native, model, failure, recovery |
//! | `E` | §36.8 effect/reset/recovery boundary | native, failure, recovery |
//! | `H` | §37.11 hook order and API compatibility | native, failure, recovery |
//! | `K` | §38 self-explanation | native, model |
//! | `N` | §40.8 development-agent norms | native |
//! | `DX` | §41.16.1 commit and generation faults | native, failure, recovery |
//! | `RT` | §41.16.2 subject, queue, backend, seat | native, failure, recovery, model |
//! | `P41` | §41.16.3 plugin and version interleaving | native, failure, recovery |
//! | `QA` | §42.7 provider quota and model selection | native, model |
//! | `UI` | §43.3 configuration and playground | native, model, failure, recovery |
//! | `CFG` | §43.3 configuration and playground | native, failure, recovery |
//! | `PG` | §43.3 configuration and playground | native, failure, recovery |
//! | `BOOT` | §44.9 first start and seed | native, failure, recovery, model |
//! | `AW` | §45.9 awareness units | native, model, failure, recovery |
//! | `DEP` | §45.10.6 dependency assembly | native, failure, recovery |
//! | `G` | §46.6 repository handover | native |
//! | `PV` | §47.7 provider protocol compatibility | native, model, failure, recovery |
//! | `UI-S` | §21.11 streaming reading stability | native |
//!
//! Cases whose row adds a scope beyond its family default are listed
//! explicitly below with the reason taken from the row.

use std::collections::BTreeMap;

use crate::coverage::Scope;

/// The declared scope set for one case, with its justification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopeDeclaration {
    pub scopes: Vec<Scope>,
    /// Why the case needs these scopes, in the SPEC's own terms.
    pub reason: String,
}

/// Family defaults, keyed by the case-ID prefix.
fn family_default(family: &str) -> (Vec<Scope>, &'static str) {
    match family {
        "A" => (
            vec![Scope::Native],
            "§23 acceptance and release blocker; needs the real product running",
        ),
        "V1" => (
            vec![Scope::Native, Scope::Model, Scope::Failure, Scope::Recovery],
            "§35.10 first-release acceptance; end-to-end developer journey",
        ),
        "E" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§36.8 effect, reset and recovery boundary",
        ),
        "H" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§37.11 hook order and API compatibility",
        ),
        "K" => (vec![Scope::Native, Scope::Model], "§38 self-explanation"),
        "N" => (vec![Scope::Native], "§40.8 development-agent working norms"),
        "DX" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§41.16.1 commit and generation faults",
        ),
        "RT" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery, Scope::Model],
            "§41.16.2 subject, queue, backend and seat faults",
        ),
        "P41" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§41.16.3 plugin and version interleaving",
        ),
        "QA" => (
            vec![Scope::Native, Scope::Model],
            "§42.7 provider quota and model selection",
        ),
        "UI" => (
            vec![Scope::Native, Scope::Model, Scope::Failure, Scope::Recovery],
            "§43.3 configuration and playground",
        ),
        "CFG" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§43.3 configuration and playground",
        ),
        "PG" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§43.3 configuration and playground",
        ),
        "BOOT" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery, Scope::Model],
            "§44.9 first start and seed",
        ),
        "AW" => (
            vec![Scope::Native, Scope::Model, Scope::Failure, Scope::Recovery],
            "§45.9 independently installable awareness",
        ),
        "DEP" => (
            vec![Scope::Native, Scope::Failure, Scope::Recovery],
            "§45.10.6 dependency assembly",
        ),
        "G" => (vec![Scope::Native], "§46.6 complete-product handover"),
        "PV" => (
            vec![Scope::Native, Scope::Model, Scope::Failure, Scope::Recovery],
            "§47.7 provider protocol compatibility",
        ),
        "UI-S" => (vec![Scope::Native], "§21.11 streaming reading stability"),
        _ => (
            Vec::new(),
            "unclassified family; required scopes remain unresolved",
        ),
    }
}

/// Cases whose row needs scopes beyond their family default.
///
/// Each entry is `(case_id, extra scopes, reason from the SPEC row)`.
const EXTRAS: &[(&str, &[Scope], &str)] = &[
    // §23 acceptance table.
    ("A05", &[Scope::Failure], "取消失联：未确认终止请求仍计占用"),
    (
        "A06",
        &[Scope::Model],
        "主体在安全边界处理消息，主体必须真正判断",
    ),
    (
        "A07",
        &[Scope::Failure, Scope::Recovery],
        "沙箱与坏包强杀后自动 LKG/factory",
    ),
    (
        "A10",
        &[Scope::Model],
        "主体跨私聊/群聊回忆，由人格判断保密",
    ),
    (
        "A11",
        &[Scope::Failure, Scope::Recovery],
        "memory commit 崩溃与冲突修复",
    ),
    ("A13", &[Scope::Model], "不点名插件的真实任务能发现并用对"),
    (
        "A14",
        &[Scope::Model],
        "正确 seat/epoch/question 的自主选择",
    ),
    (
        "A15",
        &[Scope::Failure, Scope::Recovery],
        "REPL 崩溃后的实际恢复等级",
    ),
    ("A16", &[Scope::Model], "同一主体接受音频事件并走委派链"),
    (
        "A17",
        &[Scope::Failure, Scope::Recovery],
        "Android/慢闪存/满盘下的真实恢复",
    ),
    (
        "A18",
        &[Scope::Failure, Scope::Recovery],
        "升级/卸载后无孤儿资源且可恢复",
    ),
    (
        "A21",
        &[Scope::Failure, Scope::Recovery],
        "kernel/rescue 自主替换失败后的恢复",
    ),
    ("A24", &[Scope::Model], "卸载编码插件后仍完成生活承诺"),
    ("A25", &[Scope::Model], "同意图跨渠道投影与弱渠道完成选择"),
    ("A26", &[Scope::Failure], "重复提交与旧 action 失效的判定"),
    (
        "A27",
        &[Scope::Failure, Scope::Recovery],
        "自主更新失败后的自动恢复",
    ),
    (
        "A28",
        &[Scope::Failure, Scope::Recovery],
        "配置问题进入主体与断链恢复",
    ),
    (
        "A31",
        &[Scope::Model],
        "同一 registry 能力可被主子发现并有限讨论",
    ),
    (
        "A36",
        &[Scope::Model],
        "主体逐次指定 child system/model 并判断",
    ),
    (
        "A37",
        &[Scope::Model, Scope::Failure, Scope::Recovery],
        "child 直接发布的归属与 unknown 恢复",
    ),
    (
        "A38",
        &[Scope::Model],
        "主体自主接受/拒绝 DND 并结束处理队列",
    ),
    (
        "A39",
        &[Scope::Failure, Scope::Recovery],
        "breaking-change 自迁移各阶段崩溃恢复",
    ),
    (
        "A40",
        &[Scope::Failure, Scope::Recovery],
        "半上传不得删除唯一副本",
    ),
    ("A41", &[Scope::Model], "无用户相关性仍可自主续开长期兴趣"),
    (
        "A42",
        &[Scope::Failure, Scope::Recovery, Scope::Model],
        "断线后的可达能力选择与递归接管",
    ),
    ("A44", &[Scope::Model], "多次切窗后按 ID 读原文的预算语义"),
    ("A45", &[Scope::Model], "两种 compact 独立插件的策略选择"),
    ("A46", &[Scope::Model], "初始化时用户取名或 Agent 自主取名"),
    ("A47", &[Scope::Model], "较晚节点的身份信任与合并判断"),
    (
        "A48",
        &[Scope::Failure, Scope::Recovery, Scope::Model],
        "故障证据进入主体并由 child 修复",
    ),
    (
        "A49",
        &[Scope::Failure, Scope::Recovery],
        "所有 LLM 失效后的离线镜像恢复",
    ),
    // §41.16 fault matrix rows that additionally need a model judgement.
    ("RT16", &[Scope::Model], "主体自主语义等待与条件判断"),
    // §45.10.6 dependency assembly.
    ("DEP03", &[Scope::Model], "感知与直觉指向实际承载服务"),
    // §47.7 provider protocol.
    ("PV07", &[Scope::Model], "主体感知依赖并自主定位问题"),
];

/// The declared scope set for one case.
pub fn scopes_for_case(case_id: &str) -> ScopeDeclaration {
    let family = case_family(case_id);
    let (mut scopes, reason) = family_default(family);
    if let Some((_, extra, extra_reason)) = EXTRAS.iter().find(|(id, _, _)| *id == case_id) {
        for scope in *extra {
            if !scopes.contains(scope) {
                scopes.push(*scope);
            }
        }
        scopes.sort();
        return ScopeDeclaration {
            scopes,
            reason: format!("{reason}; plus {extra_reason}"),
        };
    }
    scopes.sort();
    ScopeDeclaration {
        scopes,
        reason: reason.to_string(),
    }
}

/// Classify every case in the index.
pub fn classify(index: &crate::spec_index::SpecIndex) -> BTreeMap<String, ScopeDeclaration> {
    let mut scopes = BTreeMap::new();
    for case_id in index.acceptance_cases.keys() {
        scopes.insert(case_id.clone(), scopes_for_case(case_id));
    }
    scopes
}

/// The case-ID family prefix (`A`, `UI-S`, `V1`, `P41`, …).
pub fn case_family(case_id: &str) -> &str {
    case_id
        .trim_end_matches(|c: char| c.is_ascii_digit())
        .trim_end_matches('-')
}
