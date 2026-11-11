//! The subject's mind-shape is content, not a compiled constant — docs/specs/subject-runtime.md#spec-57-1,
//! docs/specs/subject-runtime.md#spec-57-2.
//!
//! These cases prove the mechanism with a real Node installation authority:
//! the unit that provides `cap.subject-frame` speaks for the subject, the
//! unit that provides `cap.idle-perception` words idleness, and an ambiguity
//! between two providers is reported rather than silently resolved.
//!
//! They do not claim the model behaves differently — the seed bodies are
//! verbatim today's text, so behaviour is unchanged. What changed is who owns
//! the words.

use std::path::Path;
use std::sync::Arc;

use parking_lot::Mutex;
use shenko_awareness_core::{CAP_IDLE_PERCEPTION, CAP_SUBJECT_FRAME, CapabilityUnit, CognitionCatalog};
use shenko_node::Node;
use shenko_types::ids::EntityId;

/// A scratch installation directory for one case.
fn scratch(label: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("shenko-frame-{label}-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Write one cognition unit that provides a capability contract.
fn write_unit(dir: &Path, unit_id: &str, capability: &str, body: &str) {
    let packages = dir.join("capability-units");
    std::fs::create_dir_all(&packages).unwrap();
    let entry = format!(
        "---\nid: {unit_id}\nversion: 1\nkind: awareness\ntitle: {unit_id}\ncapability_card: {unit_id}\nprovides: [{capability}]\n---\n{body}\n"
    );
    std::fs::write(packages.join(format!("{unit_id}.md")), entry).unwrap();
}

/// The installation authority over one scratch directory.
fn catalog(dir: &Path) -> Arc<CognitionCatalog> {
    let node = Arc::new(Mutex::new(Node::open(&dir.join("node")).unwrap()));
    let catalog = Arc::new(CognitionCatalog::new(node));
    shenko_awareness_core::install_dir(&catalog, &dir.join("capability-units")).unwrap();
    catalog
}

#[test]
fn an_unrelated_capability_never_resolves_the_frame() {
    let dir = scratch("absent");
    write_unit(&dir, "test.frame.absent", "cap.test-other", "别的能力。");
    let catalog = catalog(&dir);
    let resolution = catalog.capability_unit(CAP_SUBJECT_FRAME).unwrap();
    assert!(
        matches!(resolution, CapabilityUnit::Absent),
        "an unrelated capability must not resolve the frame"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn exactly_one_provider_speaks_for_the_subject() {
    let dir = scratch("one");
    write_unit(
        &dir,
        "test.frame.one",
        CAP_SUBJECT_FRAME,
        "你是自己的样子。这段话由单元提供，主体可以修订它。",
    );
    let catalog = catalog(&dir);
    let resolution = catalog.capability_unit(CAP_SUBJECT_FRAME).unwrap();
    assert_eq!(
        resolution.body(),
        Some("你是自己的样子。这段话由单元提供，主体可以修订它。")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn two_providers_are_an_ambiguity_not_a_silent_pick() {
    let dir = scratch("two");
    write_unit(&dir, "test.frame.one", CAP_SUBJECT_FRAME, "第一个。");
    write_unit(&dir, "test.frame.two", CAP_SUBJECT_FRAME, "第二个。");
    let catalog = catalog(&dir);
    let resolution = catalog.capability_unit(CAP_SUBJECT_FRAME).unwrap();
    assert!(
        resolution.is_ambiguous(),
        "two distinct providers must not be resolved by the harness"
    );
    assert_eq!(resolution.body(), None, "an ambiguous frame has no single body");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Two providers may coexist, and the subject may switch between them at
/// runtime: the consumer asks the same question and gets the provider the
/// subject left in force (docs/specs/cognition-units.md#spec-57-9 / G59).
///
/// The switch is a durable fact about the unit, not a restart: the same
/// catalog, the same call, the other provider's body.
#[test]
fn the_subject_switches_between_two_providers_without_a_restart() {
    let dir = scratch("switch");
    write_unit(&dir, "test.frame.one", CAP_SUBJECT_FRAME, "第一个。");
    write_unit(&dir, "test.frame.two", CAP_SUBJECT_FRAME, "第二个。");
    let installed = catalog(&dir);
    // Both are installed and enabled: they coexist.
    assert!(
        installed.capability_unit(CAP_SUBJECT_FRAME).unwrap().is_ambiguous(),
        "two distinct providers coexist until the subject decides"
    );
    // The subject retires one of them: the ambiguity resolves, and the
    // consumer's question is unchanged.
    installed.set_enabled("test.frame.one", false).unwrap();
    let resolution = installed.capability_unit(CAP_SUBJECT_FRAME).unwrap();
    assert_eq!(
        resolution.body(),
        Some("第二个。"),
        "the provider the subject left in force speaks for the subject"
    );
    // And the retirement is durable: a catalog opened over the same
    // installation resolves the same way.
    let reopened = catalog(&dir);
    assert_eq!(
        reopened.capability_unit(CAP_SUBJECT_FRAME).unwrap().body(),
        Some("第二个。"),
        "the switch survives a reopen: it is a fact, not a process state"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_disabled_unit_is_not_a_provider() {
    let dir = scratch("disabled");
    write_unit(&dir, "test.idle", CAP_IDLE_PERCEPTION, "[观察] 空闲。");
    let catalog = catalog(&dir);
    assert_eq!(
        catalog.capability_unit(CAP_IDLE_PERCEPTION).unwrap().body(),
        Some("[观察] 空闲。")
    );
    catalog.set_enabled("test.idle", false).unwrap();
    assert!(
        matches!(
            catalog.capability_unit(CAP_IDLE_PERCEPTION).unwrap(),
            CapabilityUnit::Absent
        ),
        "a unit the subject retired is not a provider"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
