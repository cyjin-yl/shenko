//! Explicit real CLI receipt/lifecycle verification; no mock or skipped test.
//! SHENKO_OBELISK_EXECUTABLE=<Node> SHENKO_OBELISK_SOURCE_CHECKOUT=<audited source>
//! cargo run -p shenko --example obelisk_native_receipt
use anyhow::{Context as _, Result, ensure};
use parking_lot::Mutex;
use serde_json::json;
use shenko::obelisk::{self, TASK_KIND, UNIT_ID};
use shenko_compat::{installation::plugin_export, runtime::AdapterHost};
use shenko_node::{Node, UnitRequest};
use shenko_store::Store;
use shenko_types::ids::EntityId;
use std::{path::PathBuf, sync::Arc, time::Duration};
#[path = "../tests/support/history_runtime.rs"]
mod support;
use support::{initialize, runtime, tool};

fn main() -> Result<()> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(run())
}

async fn run() -> Result<()> {
    use shenko_types::operation::EffectStatus;
    let executable =
        std::env::var_os("SHENKO_OBELISK_EXECUTABLE").context("real Node executable required")?;
    let checkout = PathBuf::from(
        std::env::var_os("SHENKO_OBELISK_SOURCE_CHECKOUT")
            .context("audited Obelisk source required")?,
    );
    let root = std::env::temp_dir().join(format!("shenko-obelisk-native-cli-{}", EntityId::new()));
    std::fs::create_dir_all(root.join("sources"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))?;
    }
    std::fs::write(
        obelisk::config_path(&root),
        serde_json::to_vec(&json!({
            "executable":PathBuf::from(executable), "source_checkout":checkout,
            "arguments":[checkout.join("packages/cli/src/obelisk.ts")],"index_home":root.join("index"),
            "source_instance":"synthetic-native-proof","sources":[{"provider":"pi","root_label":"proof","path":root.join("sources")}]
        }))?,
    )?;
    let store = Arc::new(Store::open(root.join("shenko.db"), root.join("cas"))?);
    let (parent, authority) = initialize(&store)?;
    let node = Arc::new(Mutex::new(Node::open(&root.join("node"))?));
    for id in [UNIT_ID, shenko_compat_mcp::UNIT_ID] {
        node.lock().handle_unit_request(UnitRequest::Import {
            export: plugin_export(id, true)?,
            package_root: root.join("units").display().to_string(),
            enable: Some(true),
        })?;
    }
    let (supervisor, native) = runtime(store.clone(), node.clone(), &root).await?;
    let result: Result<()> = async {
        supervisor.spawn(shenko_compat_mcp::McpPlugin, AdapterHost::new(&supervisor, store.clone(), node.clone())).await?;
        ensure!(native.has_handler("compat.artifact.read", 1) && native.has_handler(TASK_KIND, 1), "independent plugins cannot coexist");
        let query = native.call_tool(&tool(json!({"op":"obelisk","request":{"op":"sessions","limit":1}})), &parent, &authority).await?;
        ensure!(query.outcome == "succeeded", "pinned standard query did not complete: {query:?}");
        let scripted = native.call_tool(&tool(json!({"op":"obelisk","request":{"op":"script","script":"return {proof:'observed', payload:'x'.repeat(300000)};"}})), &parent, &authority).await?;
        ensure!(scripted.outcome == "unknown", "arbitrary JS exit was promoted to complete external effects");
        let operation = store.external_operation(scripted.operation_id)?;
        ensure!(operation.effect_status == EffectStatus::Unknown && operation.outcome.is_none(), "unknown effects were lost");
        let evidence = store.external_operation_evidence(scripted.operation_id)?;
        ensure!(evidence.len() == 1, "script did not preserve exactly one original E1");
        let normalized_ref = evidence[0].normalized_ref.as_ref().context("normalized script E1")?;
        let normalized: serde_json::Value = serde_json::from_slice(&store.cas.read_object(&normalized_ref.digest)?)?;
        ensure!(normalized["observation"]["status"] == "observation_outlined", "large observation was not outlined");
        ensure!(normalized["observation"]["read"]["handler"] == TASK_KIND && normalized["observation"]["read"]["input"]["op"] == "artifact", "outline points to another plugin's reader");
        node.lock().handle_unit_request(UnitRequest::SetEnabled { unit_id: shenko_compat_mcp::UNIT_ID.into(), enabled: false })?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while native.has_handler("compat.artifact.read", 1) { tokio::time::sleep(Duration::from_millis(20)).await; }
        }).await?;
        let read = native.call_tool(&tool(json!({"op":"artifact","digest":normalized["source_receipt"]["digest"],"limit":128})), &parent, &authority).await?;
        ensure!(read.outcome == "succeeded", "MCP retirement removed history receipt reader: {read:?}");
        let payload = &read.result.as_ref().context("artifact result")?["observation"]["result"];
        ensure!(payload["total_bytes"].as_u64().is_some_and(|n|n > 300000) && payload["next_offset"] == 128, "original outlined observation is not range-readable: {payload}");
        let original = obelisk::handler(store.clone(), node.clone(), obelisk::config_path(&root))
            .reconcile(&store.external_task(scripted.task_id)?, scripted.operation_id).await?;
        ensure!(original.effect_status == EffectStatus::Unknown && store.external_operation_evidence(scripted.operation_id)?.len() == 1
            && store.external_operation_attempts(scripted.operation_id)?.len() == 1, "reconciliation replayed JS or rewrote E1");
        Ok(())
    }.await;
    let cleanup = supervisor.stop().await;
    if result.is_ok() && cleanup.is_ok() {
        let _ = std::fs::remove_dir_all(&root);
    }
    result.and(cleanup)
}
