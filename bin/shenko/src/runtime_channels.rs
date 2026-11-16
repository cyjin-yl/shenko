//! Configured channel registry, capability disclosure and shared admission facts.
use std::{path::PathBuf, sync::Arc};

use anyhow::{Context, Result};

use crate::{runtime_adapters, runtime_napcat};
use crate::runtime_napcat::NapCatOptions;

pub(crate) struct ChannelConfig {
    pub(crate) napcat: NapCatOptions,
    pub(crate) channels_config: Option<PathBuf>,
}

pub(crate) struct ChannelServices {
    pub(crate) registry: shenko_types::channel::ChannelRegistry,
    _contacts_plugin: Option<cordis_core::FiberHandle>,
}

pub(crate) async fn install(
    supervisor: &shenko_kernel::Supervisor,
    store: &Arc<shenko_store::Store>,
    data_dir: &PathBuf,
    agent_id: shenko_types::ids::EntityId,
    subject_inbox: &Arc<shenko_subject::inbox::Inbox>,
    self_knowledge: &Arc<shenko_awareness_core::SelfKnowledgeProvider>,
    consumer_cancel: &tokio_util::sync::CancellationToken,
    config: ChannelConfig,
) -> Result<ChannelServices> {
    // The IM reply boundary ships as channel plugins: the live prompt
    // and parser advertise exactly the channels whose transport is
    // configured. With no NapCat endpoint no reply channel exists, so
    // the contract cannot promise an undeliverable reply. Plugins
    // register into this registry once their connector exists.
    let mut channel_registry = shenko_types::channel::ChannelRegistry::new();

    let contacts_plugin = runtime_napcat::install(
        supervisor, store, data_dir, agent_id, subject_inbox,
        &mut channel_registry, consumer_cancel, config.napcat,
    ).await?;
    runtime_adapters::install(
        store, data_dir, agent_id, subject_inbox,
        &mut channel_registry, consumer_cancel, config.channels_config,
    ).await?;
    // Every installed channel declares what its platform can really
    // do, and the runtime records that declaration as a durable fact
    // the subject can read (RT19). The table comes
    // from the plugin contract, so the QQ plugin is described the
    // same way as the adapters: a capability nobody can reach is not
    // advertised, and an advertised one is reachable.
    for (channel, account, capabilities) in channel_registry.capability_tables() {
        store.record_channel_fact(&shenko_store::channel::NewChannelFact {
            channel,
            account,
            kind: "capabilities".to_string(),
            state: "declared".to_string(),
            subject_ref: None,
            detail: serde_json::to_value(&capabilities)?,
        })?;
    }
    // The same declaration reaches the subject's own self-knowledge
    // surface, so it can read what each installed channel really
    // offers before it promises a reply or an action
    //
    self_knowledge.install_channel_capabilities(&channel_registry.capability_tables());

    Ok(ChannelServices { registry: channel_registry, _contacts_plugin: contacts_plugin })
}

/// Resolve one classified envelope's attachment locators through the owning
/// plugin, then seal the delivered bytes as immutable content.
///
/// The plugin owns the platform mechanism: an installed account's
/// authenticated media call, or a read inside the media root of the host that
/// owns the bytes. The Store owns content addressing, so the caller seals
/// exactly the bytes the provider delivered and points the attachment at the
/// content digest; the provider identity (`attachment_id`, `kind`, `name`,
/// `locator`) is preserved as classified.
///
/// A locator that cannot be resolved keeps its locator — the bytes are still
/// where the platform said — and the failure is recorded as a typed,
/// subject-readable `attachment_locator` fact: the reason token says whether
/// the credential, the platform's lifetime, the byte bound or the transport
/// was responsible. The message itself is never dropped, and no accepted fact
/// is rewritten. Shared by the napcat spool admission and the adapter-source
/// admission.
pub(super) async fn prepare_channel_envelope(
    plugin: &Arc<dyn shenko_types::channel::ChannelPlugin>,
    store: &shenko_store::store::Store,
    event_id: shenko_types::ids::EntityId,
    mut envelope: shenko_types::channel::ChannelEnvelope,
) -> Result<shenko_types::channel::ChannelEnvelope> {
    for index in 0..envelope.attachments.len() {
        let attachment = envelope.attachments[index].clone();
        match plugin.resolve_attachment(&attachment).await {
            Ok(resolved) => {
                let mut attachment = resolved.attachment;
                let media_type = attachment
                    .mime
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_string());
                let sealed = store
                    .cas
                    .seal_object(&resolved.bytes, &media_type, "channel_attachments", "bytes", 1)
                    .context("seal resolved channel attachment bytes")?;
                attachment.object = Some(sealed.object);
                envelope.attachments[index] = attachment;
            }
            Err(error) => {
                store.record_channel_fact(&shenko_store::channel::NewChannelFact {
                    channel: envelope.provider.clone(),
                    account: envelope.account.clone(),
                    kind: "attachment_locator".to_string(),
                    state: "pending".to_string(),
                    subject_ref: Some(event_id.to_string()),
                    detail: serde_json::json!({
                        "attachment_id": attachment.attachment_id,
                        "reason": error.token(),
                        "detail": error.to_string(),
                    }),
                })?;
            }
        }
    }
    Ok(envelope)
}


/// Journal one channel-runtime error as a durable fact of the subject's
/// history. The journal entry is NOT a delivery receipt: it
/// never retries the operation and never opens an issue by itself.
/// Independent operation unknown/conflict receipts stay on the operation
/// ledger; this only makes the unable-to-continue boundary readable.
pub(super) fn journal_channel_error(
    store: &shenko_store::store::Store,
    agent_id: shenko_types::ids::EntityId,
    kind: &str,
    source_module: &str,
    message: String,
    extra: serde_json::Value,
) {
    store.journal_runtime_error(
        agent_id,
        shenko_store::raw_session::RuntimeErrorContext {
            kind: kind.to_string(),
            source_module: source_module.to_string(),
            message,
            step_no: None,
            task_id: None,
            extra,
        },
    );
}
