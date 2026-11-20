//! Scoped native consumer: original upload -> staged history -> S0 -> wire.
//! Loopback protocol fixtures establish mechanism, never model efficacy.
use super::{admit_web_event, fixture, message, state};
use crate::context_projection::prepare_context;
use base64::Engine as _;
use shenko_broker::{broker::Broker, provider::AnthropicProvider, request::*};
use shenko_types::{ids::*, object::Digest};
use std::sync::Arc;

fn png_bytes() -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut bytes, 29, 15);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(&vec![91; 29 * 15 * 4]).unwrap();
    }
    bytes
}

fn caps(binding: ProviderBinding) -> ProviderCapabilities {
    ProviderCapabilities { binding, max_context_tokens: Some(32768),
        vision: Some(ProviderVisionCaps { max_image_context: 32768, patch_px: 14 }) }
}

fn image_count(payload: &InferencePayload) -> usize {
    payload.messages.iter().flat_map(|message| &message.parts).filter(|part| part.is_image()).count()
}

fn retained_original(store: &shenko_store::Store, digest: &Digest) {
    // Main's canonical read-only inventory verifies this original's committed
    // reference and bytes. It does not establish a GC exclusion/deletion epoch.
    let snapshot = store.retained_object_snapshot(shenko_store::RetentionLimits::default()).unwrap();
    assert!(snapshot.references.iter().any(|reference| &reference.digest == digest),
        "durable input must still reference the original attachment");
    assert!(snapshot.verified_objects.contains(digest), "the original attachment must remain byte-verified");
    assert!(!snapshot.external_or_absent_digests.contains(digest));
}

#[tokio::test]
async fn subject_attachment_png_original_survives_staging_reopen_seal_and_wire() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let provider = AnthropicProvider::with_endpoint("offline-fixture".into(), endpoint, "fixture");
    let binding = provider.binding();
    let (_broker, handle) = Broker::with_anthropic_provider(provider.clone(), 1).unwrap();
    let (dir, store, agent) = fixture();
    let original = png_bytes();
    let attachment = store.seal_web_attachment(&original, "original.png", "image/png").unwrap();
    let mut event = message("保留原始附件及重复引用");
    event.payload["attachment_refs"] = serde_json::json!([attachment, attachment]);
    admit_web_event(&store, agent, "png-original", &event);
    retained_original(&store, &attachment.digest);
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(caps(binding.clone())));
    subject.max_tokens = 64;
    let prepared = prepare_context(&mut subject, &[event.clone()]).unwrap().unwrap();
    assert_eq!(image_count(&prepared.payload), 1, "same digest has one actual image input");
    assert_eq!(prepared.context.payload_layout.as_ref().unwrap().vision_tokens, 6);
    assert_eq!(prepared.context.estimated_input_tokens,
        crate::context_projection::estimate_payload_tokens(&prepared.payload, 6));
    let before = serde_json::to_value(&prepared.payload).unwrap();
    assert_eq!(store.web_input_selected_step(agent, event.event_id).unwrap(), None);
    retained_original(&store, &attachment.digest);
    drop((prepared, subject, store, _broker));

    let store = Arc::new(shenko_store::Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (_broker, handle) = Broker::with_anthropic_provider(provider.clone(), 1).unwrap();
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(caps(binding.clone())));
    subject.max_tokens = 64;
    let replay = prepare_context(&mut subject, &[]).unwrap().unwrap();
    assert_eq!(serde_json::to_value(&replay.payload).unwrap(), before);
    assert_eq!(replay.events, vec![event.clone()]);
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    super::execute_step(subject.step.step_no, &[], &mut subject, &cancel).await.unwrap();
    let frozen = super::super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(serde_json::to_value(&frozen.payload).unwrap(), before);
    assert_eq!(frozen.provider_binding.as_ref(), Some(&binding));
    assert_eq!(frozen.context.capabilities.as_ref().unwrap().binding, binding);
    assert_eq!(store.web_input_selected_step(agent, event.event_id).unwrap(), Some(subject.step.step_id));
    let seal = subject.step.input_plan_ref.clone().unwrap();
    let frozen_bytes = store.cas.read_object(&seal.digest).unwrap();
    retained_original(&store, &attachment.digest);
    drop((frozen, subject, store, _broker));

    let store = Arc::new(shenko_store::Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let (_, step) = store.load_subject(agent).unwrap();
    let frozen = super::super::frozen_input(&store, &step).unwrap();
    assert_eq!(store.cas.read_object(&seal.digest).unwrap(), frozen_bytes);
    assert_eq!(serde_json::to_value(&frozen.payload).unwrap(), before);
    retained_original(&store, &attachment.digest);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut bytes = Vec::new();
        let (start, length) = loop {
            let mut chunk = [0; 4096];
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0);
            bytes.extend_from_slice(&chunk[..read]);
            if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                let headers = std::str::from_utf8(&bytes[..end]).unwrap();
                let length: usize = headers.lines().find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                }).unwrap();
                if bytes.len() >= end + 4 + length { break (end + 4, length); }
            }
        };
        let body: serde_json::Value = serde_json::from_slice(&bytes[start..start + length]).unwrap();
        let raw = r#"{"content":[{"type":"text","text":"fixture"}],"stop_reason":"end_turn"}"#;
        socket.write_all(format!("HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{raw}", raw.len()).as_bytes()).await.unwrap();
        body
    });
    let (result_tx, _result_rx) = tokio::sync::oneshot::channel();
    tokio::time::timeout(std::time::Duration::from_secs(10), provider.complete(&InferenceRequest { request_id: frozen.request_id, agent_id: agent,
        conversation_id: EntityId(step.branch_id.0), provider_binding: binding,
        priority: InferencePriority::Subject, epoch: step.controller_epoch, payload: frozen.payload, result_tx })).await.unwrap().unwrap();
    let body = tokio::time::timeout(std::time::Duration::from_secs(10), server).await.unwrap().unwrap();
    let images: Vec<_> = body["messages"].as_array().unwrap().iter()
        .flat_map(|message| message["content"].as_array().unwrap())
        .filter(|part| part["type"] == "image").collect();
    assert_eq!(images.len(), 1);
    assert_eq!(images[0]["source"]["media_type"], "image/png");
    let wire = base64::engine::general_purpose::STANDARD.decode(images[0]["source"]["data"].as_str().unwrap()).unwrap();
    assert_eq!(wire, original);
    assert_eq!(Digest::blake3(&wire), attachment.digest);
}

#[test]
fn subject_attachment_unknown_nonvision_unvalidated_and_other_ingress_stay_text() {
    let (_dir, store, agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let binding = handle.binding().clone();
    let valid = store.seal_web_attachment(&png_bytes(), "valid.png", "image/png").unwrap();
    let bad = store.seal_web_attachment(b"not a raster", "bad.png", "image/png").unwrap();
    let jpeg = store.seal_web_attachment(b"declared jpeg", "photo.jpg", "image/jpeg").unwrap();
    let mut event = message("附件事实");
    event.payload["attachment_refs"] = serde_json::json!([valid, bad, jpeg]);
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle);
    subject.max_tokens = 64;
    for capabilities in [None, Some(ProviderCapabilities { vision: None, ..caps(binding.clone()) }),
        Some(ProviderCapabilities { vision: Some(ProviderVisionCaps { patch_px: 0, max_image_context: 32768 }), ..caps(binding.clone()) })] {
        subject.model_capabilities = capabilities.map(Arc::new);
        let prepared = prepare_context(&mut subject, &[event.clone()]).unwrap().unwrap();
        assert_eq!(image_count(&prepared.payload), 0);
        assert_eq!(prepared.context.payload_layout.unwrap().vision_tokens, 0);
        assert!(serde_json::to_string(&prepared.payload).unwrap().contains("看不到画面"));
    }
    subject.model_capabilities = Some(Arc::new(caps(binding)));
    let prepared = prepare_context(&mut subject, &[event.clone()]).unwrap().unwrap();
    assert_eq!(image_count(&prepared.payload), 1);
    let rendered = serde_json::to_string(&prepared.payload).unwrap();
    assert!(rendered.contains("未通过有界静态栅格解码验证"));
    assert!(rendered.contains("未经当前 PNG 验证路径验证"));
    let ordinary = shenko_types::inbox::InboxEvent { source: "other".into(), ..event };
    let (_other_dir, other_store, other_agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let capabilities = caps(handle.binding().clone());
    let mut other = state(other_store, other_agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
    other.max_tokens = 64;
    assert_eq!(image_count(&prepare_context(&mut other, &[ordinary]).unwrap().unwrap().payload), 0);
}

#[test]
fn subject_attachment_image_budget_and_exact_provider_binding_reject_before_seal() {
    let (_dir, store, agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let binding = handle.binding().clone();
    let attachment = store.seal_web_attachment(&png_bytes(), "budget.png", "image/png").unwrap();
    let mut event = message("图像预算");
    event.payload["attachment_refs"] = serde_json::json!([attachment]);
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle).with_model_capabilities(Some(caps(binding.clone())));
    subject.max_tokens = 64;
    let prepared = prepare_context(&mut subject, &[event.clone()]).unwrap().unwrap();
    let charge = prepared.context.budget_input_tokens.unwrap();
    subject.model_capabilities = Some(Arc::new(ProviderCapabilities {
        vision: Some(ProviderVisionCaps { max_image_context: charge + 63, patch_px: 14 }), ..caps(binding)
    }));
    assert!(prepare_context(&mut subject, &[event.clone()]).is_err());
    assert!(store.load_subject(agent).unwrap().1.input_plan_ref.is_none());
    assert!(store.web_input_selected_step(agent, event.event_id).unwrap().is_none());
    let mut mismatched = (*subject.model_capabilities.as_ref().unwrap().as_ref()).clone();
    mismatched.binding = ProviderBinding::Stub { codec: 12345 };
    subject.model_capabilities = Some(Arc::new(mismatched));
    assert!(prepare_context(&mut subject, &[event]).is_err());
}

fn chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&u32::try_from(data.len()).unwrap().to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut crc = crc32fast::Hasher::new();
    crc.update(kind);
    crc.update(data);
    output.extend_from_slice(&crc.finalize().to_be_bytes());
}

#[test]
fn subject_attachment_excess_idat_is_rejected_under_parent_process_deadline() {
    use std::io::Write as _;
    // Valid chunk CRCs, tiny declared raster (five filtered bytes), but a
    // compressed stream expanding beyond the entire attachment memory bound.
    let mut compressed = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::fast());
    let block = [0; 8192];
    for _ in 0..(65 * 1024 * 1024 / block.len()) { compressed.write_all(&block).unwrap(); }
    let compressed = compressed.finish().unwrap();
    let mut bomb = b"\x89PNG\r\n\x1a\n".to_vec();
    let header = [0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0];
    chunk(&mut bomb, b"IHDR", &header);
    chunk(&mut bomb, b"IDAT", &compressed);
    chunk(&mut bomb, b"IEND", &[]);
    // Parent owns the process deadline, termination and reap evidence. An
    // in-process spawn_blocking timeout cannot establish those guarantees.
    let (_dir, store, agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let capabilities = caps(handle.binding().clone());
    let attachment = store.seal_web_attachment(&bomb, "excess.png", "image/png").unwrap();
    let mut event = message("过量压缩数据");
    event.payload["attachment_refs"] = serde_json::json!([attachment]);
    let mut subject = state(store, agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
    subject.max_tokens = 64;
    let prepared = prepare_context(&mut subject, &[event]).unwrap().unwrap();
    assert_eq!(image_count(&prepared.payload), 0);
    assert!(serde_json::to_string(&prepared.payload).unwrap().contains("未通过有界静态栅格解码验证"));
}

#[test]
fn subject_attachment_iend_and_ancillary_checksums_are_strict() {
    let (_dir, store, agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let capabilities = caps(handle.binding().clone());
    let valid = png_bytes();
    let mut corrupt = valid.clone();
    *corrupt.last_mut().unwrap() ^= 1;
    let truncated = valid[..valid.len() - 2].to_vec();
    let iend = valid.len() - 12;
    let mut ancillary = valid[..iend].to_vec();
    chunk(&mut ancillary, b"tEXt", b"note\0post-IDAT ancillary control");
    let mut corrupt_ancillary = ancillary.clone();
    *corrupt_ancillary.last_mut().unwrap() ^= 1;
    ancillary.extend_from_slice(&valid[iend..]);
    corrupt_ancillary.extend_from_slice(&valid[iend..]);
    let references: Vec<_> = [corrupt, truncated, ancillary, corrupt_ancillary].into_iter().enumerate().map(|(index, bytes)| {
        store.seal_web_attachment(&bytes, &format!("checksum-{index}.png"), "image/png").unwrap()
    }).collect();
    let mut event = message("完整结束块验证");
    event.payload["attachment_refs"] = serde_json::to_value(references).unwrap();
    let mut subject = state(store, agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
    subject.max_tokens = 64;
    let prepared = prepare_context(&mut subject, &[event]).unwrap().unwrap();
    assert_eq!(image_count(&prepared.payload), 1, "valid ancillary control admits; bad critical/ancillary CRCs stay text");
    assert_eq!(prepared.context.payload_layout.unwrap().vision_tokens, 6);
    assert_eq!(serde_json::to_string(&prepared.payload).unwrap().matches("未通过有界静态栅格解码验证").count(), 3);
}

#[tokio::test]
async fn subject_attachment_plain_history_survives_with_typed_png_but_malformed_envelopes_fail() {
    let (dir, store, agent) = fixture();
    let (_broker, handle) = Broker::offline().unwrap();
    let capabilities = caps(handle.binding().clone());
    let mut subject = state(store.clone(), agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
    subject.max_tokens = 64;
    let attachment = store.seal_web_attachment(&png_bytes(), "alongside.png", "image/png").unwrap();
    let mut event = message("普通历史与图片同时保留");
    event.payload["attachment_refs"] = serde_json::json!([attachment]);
    prepare_context(&mut subject, &[event]).unwrap().unwrap();

    let scope = EntityId(subject.head.branch_id.0);
    let mut batch = shenko_types::store::WriteBatch::new();
    let seq = store.session_leaf_seq(scope).unwrap().map_or(0, |seq| seq + 1);
    batch.session_event_upserts.push(shenko_types::store::RawSessionEvent {
        event_id: EntityId::new(), session_id: scope, seq, event_type: "user".into(), role: "user".into(),
        data: "ordinary non-JSON original user history".into(),
    });
    store.commit(batch).unwrap();
    let prepared = prepare_context(&mut subject, &[]).unwrap().unwrap();
    assert_eq!(image_count(&prepared.payload), 1);
    assert!(serde_json::to_string(&prepared.payload).unwrap().contains("ordinary non-JSON original user history"));
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    super::execute_step(subject.step.step_no, &[], &mut subject, &cancel).await.unwrap();
    let sealed = super::super::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(image_count(&sealed.payload), 1);
    assert!(serde_json::to_string(&sealed.payload).unwrap().contains("ordinary non-JSON original user history"));
    drop((prepared, sealed, subject, store, _broker));
    let reopened = shenko_store::Store::open(dir.join("store.db"), dir.join("cas")).unwrap();
    let (_, step) = reopened.load_subject(agent).unwrap();
    let recovered = super::super::frozen_input(&reopened, &step).unwrap();
    assert_eq!(image_count(&recovered.payload), 1);
    assert!(serde_json::to_string(&recovered.payload).unwrap().contains("ordinary non-JSON original user history"));
    // Once a JSON inbox envelope exists, malformed fields remain errors.
    let (_other_dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    store.append_subject_raw_fact(&subject.head, EntityId::new(), "user", &serde_json::json!({
        "v": 1, "text": "malformed typed envelope", "inbox_event": { "source": "web_ui" }
    })).unwrap();
    assert!(prepare_context(&mut subject, &[]).is_err());
}

#[test]
fn subject_attachment_negative_cache_keeps_repeated_length_and_typed_array_strictness() {
    for vision in [false, true] {
        let (_dir, store, agent) = fixture();
        let (_broker, handle) = Broker::offline().unwrap();
        let mut capabilities = caps(handle.binding().clone());
        if !vision { capabilities.vision = None; }
        let bad = store.seal_web_attachment(b"invalid PNG", "invalid.png", "image/png").unwrap();
        let conflicting = shenko_store::web_attachment::WebAttachment { bytes: bad.bytes + 1, ..bad.clone() };
        let mut event = message("重复无效 PNG 的长度必须一致");
        event.payload["attachment_refs"] = serde_json::json!([bad, conflicting]);
        let mut subject = state(store, agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
        subject.max_tokens = 64;
        let error = prepare_context(&mut subject, &[event]).err().expect("conflicting repeated length must reject even after failed decode");
        assert!(error.to_string().contains("conflicting admitted length"));
    }
    let (_dir, store, agent) = fixture();
    let mut subject = state(store, agent).unwrap();
    let mut event = message("typed array required");
    event.payload["attachment_refs"] = serde_json::json!("not an array");
    assert!(prepare_context(&mut subject, &[event]).is_err());
}

fn assert_first_cas_length_and_existence(vision: bool, media_type: &str) {
    for missing in [false, true] {
        let (_dir, store, agent) = fixture();
        let (_broker, handle) = Broker::offline().unwrap();
        let mut capabilities = caps(handle.binding().clone());
        if !vision {
            capabilities.vision = None;
        }
        let attachment = if missing {
            shenko_store::web_attachment::WebAttachment {
                digest: Digest::blake3(b"original never sealed"), name: "missing.png".into(),
                media_type: media_type.into(), bytes: 21,
            }
        } else {
            let mut reference = store.seal_web_attachment(&png_bytes(), "wrong-length.png", media_type).unwrap();
            reference.bytes += 1;
            reference.media_type = media_type.into();
            reference
        };
        let mut event = message("一致但错误的声明不能成为已验证身份");
        event.payload["attachment_refs"] = serde_json::json!([attachment, attachment]);
        let mut subject = state(store, agent).unwrap().with_broker(handle).with_model_capabilities(Some(capabilities));
        subject.max_tokens = 64;
        let error = prepare_context(&mut subject, &[event]).err()
            .expect("the first alias must match existing CAS length before any fallback");
        assert!(error.to_string().contains("original missing or differs from admitted length"));
        assert!(subject.step.input_plan_ref.is_none());
    }
}

#[test]
fn subject_attachment_first_cas_length_and_existence_precede_every_image_fallback_vision_disabled_png() {
    assert_first_cas_length_and_existence(false, "image/png");
}

#[test]
fn subject_attachment_first_cas_length_and_existence_precede_every_image_fallback_vision_disabled_jpeg() {
    assert_first_cas_length_and_existence(false, "image/jpeg");
}

#[test]
fn subject_attachment_first_cas_length_and_existence_precede_every_image_fallback_vision_enabled_png() {
    assert_first_cas_length_and_existence(true, "image/png");
}

#[test]
fn subject_attachment_first_cas_length_and_existence_precede_every_image_fallback_vision_enabled_jpeg() {
    assert_first_cas_length_and_existence(true, "image/jpeg");
}
