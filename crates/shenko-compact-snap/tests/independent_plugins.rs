//! Snapcompact and budget compact as two independent Cordis plugins: honest
//! capability gating, fallback, and uninstalling one without the other.
use std::sync::Arc;

use shenko_compact_budget::{BudgetCompactPlugin, BudgetConfig};
use shenko_compact_snap::{SnapCompactPlugin, SnapConfig};
use shenko_context_policy::{
    Availability, ContextPolicyHost, ItemKind, MemoryHistory, ModelCaps, PolicyError,
    PolicyRegistry, VisionCaps, WindowEntry,
};
use shenko_kernel::{FiberHandle, PluginRegistry};
use tokio_util::sync::CancellationToken;

fn history(text: impl Fn(usize) -> String) -> MemoryHistory {
    let history = MemoryHistory::new();
    history.append(ItemKind::System, "system prompt", 100);
    for n in 0..60 {
        history.append(ItemKind::User, text(n), 200);
        history.append(ItemKind::ToolCall {
            call_id: format!("c{n}"),
            tool: "shell".into(),
            args_ref: format!("ledger://call/c{n}"),
            status: shenko_context_policy::CallStatus::Executed { ok: true },
        }, "cargo test -p shenko-store", 20);
        history.append(ItemKind::ToolResult {
            call_id: format!("c{n}"),
            status: shenko_context_policy::CallStatus::Executed { ok: true },
        }, "x".repeat(5000), 1200);
    }
    history
}

const VISION: ModelCaps = ModelCaps {
    model_id: String::new(),
    max_context: 131072,
    vision: Some(VisionCaps { max_image_context: 32768, patch_px: 28 }),
};

/// Host plus both strategies; returns snapcompact's Fiber for disposal.
async fn host_with_both() -> (PluginRegistry, Arc<PolicyRegistry>, FiberHandle) {
    let host = PluginRegistry::new(CancellationToken::new());
    let registry = Arc::new(PolicyRegistry::new());
    host.spawn(ContextPolicyHost, registry.clone()).await.unwrap();
    let snap = host.spawn(SnapCompactPlugin, SnapConfig::default()).await.unwrap();
    host.spawn(BudgetCompactPlugin, BudgetConfig::default()).await.unwrap();
    registry.set_preference(vec!["snapcompact".into(), "budget-compact".into()]);
    (host, registry, snap)
}

#[tokio::test(flavor = "multi_thread")]
async fn without_usable_vision_snapcompact_reports_and_budget_takes_over() {
    let (host, registry, _snap) = host_with_both().await;
    let text_only = ModelCaps { model_id: "text-only".into(), max_context: 131072, vision: None };
    let h = history(|n| format!("step {n}: fix path crates/shenko-store/src/lib.rs:{n}"));
    let items = h.snapshot();
    let used = items.iter().map(|i| i.tokens).sum();

    let (window, attempts) = registry.prepare(&items, &text_only, used).unwrap();
    assert!(matches!(&attempts[0].outcome, Err(PolicyError::Unavailable(_))));
    assert_eq!(window.policy.id, "budget-compact");

    // Vision present, but the archive is Chinese and the frame font has no CJK
    // glyphs: reported unreadable, never shipped as a frame, budget falls in.
    let cjk = history(|n| format!("第{n}步：修复存储层的恢复路径"));
    let items = cjk.snapshot();
    let (window, attempts) = registry.prepare(&items, &VISION, used).unwrap();
    assert!(matches!(&attempts[0].outcome, Err(PolicyError::FrameUnreadable(_))));
    assert_eq!(window.policy.id, "budget-compact");
    host.stop().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn snapcompact_renders_frames_and_uninstalls_independently() {
    let (host, registry, snap) = host_with_both().await;
    let h = history(|n| format!("step {n}: fix path crates/shenko-store/src/lib.rs:{n}"));
    let items = h.snapshot();
    let used = items.iter().map(|i| i.tokens).sum();
    assert!(registry.availability(&VISION).iter().all(|(_, a)| *a == Availability::Available));

    let (prepared, _) = registry.prepare(&items, &VISION, used).unwrap();
    assert_eq!(prepared.policy.id, "snapcompact");
    assert!(prepared.cost.window_tokens <= 32768, "fits the image-request envelope");
    assert!(prepared.cost.truncated_chars > 0, "bounded archive records truncation");
    assert!(prepared.cost.vision_tokens_estimate > 0 && prepared.cost.image_bytes > 0);
    let frame = &prepared.derivatives[0];
    assert_eq!(&frame.bytes[1..4], b"PNG");
    // Every frame is introduced by a pointer back to its raw range.
    let first_image = prepared.entries.iter().position(|e| matches!(e, WindowEntry::Image { .. }));
    assert!(matches!(prepared.entries[first_image.unwrap() - 1], WindowEntry::Pointer { .. }));
    let window = registry.commit(prepared, &h).unwrap();

    // Uninstalling snapcompact leaves budget compact and the committed frames intact.
    snap.dispose().await.unwrap();
    let ids: Vec<_> = registry.installed().into_iter().map(|(m, _)| m.id).collect();
    assert_eq!(ids, vec!["budget-compact".to_string()]);
    assert_eq!(registry.ledger().current().derivatives, window.derivatives);
    h.append(ItemKind::User, "after uninstall", 10);
    let items = h.snapshot();
    let (next, _) = registry.prepare(&items, &VISION, used).unwrap();
    assert_eq!(next.policy.id, "budget-compact");
    registry.commit(next, &h).unwrap();
    host.stop().await.unwrap();
}
