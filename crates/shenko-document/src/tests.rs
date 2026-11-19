//! Structural tests for the document.parse boundary. These prove the
//! contract shapes and the honest-Unavailable behavior — they are NOT a real
//! OXR parse (which needs GPU + model weights + the engine, per issue #89).

use super::*;

fn request(pages: &[u32]) -> ParseRequest {
    ParseRequest {
        schema_version: DOCUMENT_PARSE_REQUEST_SCHEMA_VERSION,
        source_digest: "blake3:deadbeef".into(),
        source_name: Some("doc.pdf".into()),
        pages: pages.iter().map(|i| PageRef { page_index: *i }).collect(),
        closure_id: "oxr/1.0".into(),
        cache_scope: Some("subject-a:scope-a".into()),
        deployment_scope: Some("scope-a".into()),
    }
}

fn unbound_provider() -> std::sync::Arc<dyn DocumentProvider> {
    std::sync::Arc::new(OxrProvider::new(OxrProviderConfig::default()))
}

#[test]
fn page_cache_key_binds_exact_inputs() {
    let req = request(&[0]);
    let k0 = page_cache_key(&req, PageRef { page_index: 0 });
    // Same inputs -> same key.
    assert_eq!(k0, page_cache_key(&req, PageRef { page_index: 0 }));
    // A different page is a different key.
    assert_ne!(k0, page_cache_key(&req, PageRef { page_index: 1 }));
    // A different scope never mixes results.
    let mut other_scope = req.clone();
    other_scope.deployment_scope = Some("scope-b".into());
    assert_ne!(k0, page_cache_key(&other_scope, PageRef { page_index: 0 }));
    let mut other_partition = req.clone();
    other_partition.cache_scope = Some("subject-b:scope-a".into());
    assert_ne!(k0, page_cache_key(&other_partition, PageRef { page_index: 0 }));
    let mut other_name = req.clone();
    other_name.source_name = Some("same-bytes.png".into());
    assert_ne!(k0, page_cache_key(&other_name, PageRef { page_index: 0 }));
    let mut absent_name = req.clone();
    absent_name.source_name = None;
    let mut empty_name = absent_name.clone();
    empty_name.source_name = Some(String::new());
    assert_ne!(page_cache_key(&absent_name, PageRef { page_index: 0 }), page_cache_key(&empty_name, PageRef { page_index: 0 }));
    let mut other_source = req.clone();
    other_source.source_digest = "blake3:different-source".into();
    assert_ne!(k0, page_cache_key(&other_source, PageRef { page_index: 0 }));
    let mut other_schema = req.clone();
    other_schema.schema_version += 1;
    assert_ne!(k0, page_cache_key(&other_schema, PageRef { page_index: 0 }));
    let mut absent_scope = req.clone();
    absent_scope.deployment_scope = None;
    let mut empty_scope = absent_scope.clone();
    empty_scope.deployment_scope = Some(String::new());
    assert_ne!(page_cache_key(&absent_scope, PageRef { page_index: 0 }), page_cache_key(&empty_scope, PageRef { page_index: 0 }));
    absent_scope = req.clone();
    absent_scope.cache_scope = None;
    empty_scope = absent_scope.clone();
    empty_scope.cache_scope = Some(String::new());
    assert_ne!(page_cache_key(&absent_scope, PageRef { page_index: 0 }), page_cache_key(&empty_scope, PageRef { page_index: 0 }));
    // A different closure is a different key.
    let mut other_closure = req.clone();
    other_closure.closure_id = "other/2.0".into();
    assert_ne!(
        k0,
        page_cache_key(&other_closure, PageRef { page_index: 0 })
    );
}

#[test]
fn parse_request_id_is_deterministic() {
    let a = parse_request_id(&request(&[0, 1]));
    let b = parse_request_id(&request(&[0, 1]));
    let c = parse_request_id(&request(&[0, 2]));
    assert_eq!(a, b, "same request -> same durable id");
    assert_ne!(a, c, "different pages -> different id");
}

#[tokio::test]
async fn unprovisioned_oxr_reports_unavailable_not_a_substitute() {
    let provider = unbound_provider();
    assert_eq!(
        provider.availability().await,
        ProviderAvailability::Unavailable {
            reason: "no OXR endpoint bound (endpoint is None)".into()
        }
    );
    // run_parse reports ProviderUnavailable with NO page claims.
    let mut req = request(&[0, 1]);
    req.closure_id = provider.closure_id().into();
    req.deployment_scope = provider.scope().map(str::to_owned);
    let run = run_parse(
        &req,
        &provider,
        b"%PDF-1.7",
        ParseOptions::default(),
    )
    .await
    .unwrap();
    assert_eq!(run.state, ParseState::ProviderUnavailable);
    assert!(
        run.checkpoints.is_empty(),
        "no page result may be fabricated"
    );
}

#[tokio::test]
async fn model_identity_missing_is_not_ready() {
    // Endpoint + credential bound, but the model identity is missing: a page
    // could never name the model that produced it, so not Ready.
    let provider = OxrProvider::new(OxrProviderConfig {
        endpoint: Some("http://127.0.0.1:8000/v1".into()),
        credential: Some(CredentialHandle::new("dummy")),
        ..OxrProviderConfig::default()
    });
    match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("model"), "{reason}");
        }
        ProviderAvailability::Ready => panic!("a missing model identity is never ready"),
    }
}

#[tokio::test]
async fn credential_missing_is_not_ready() {
    let provider = OxrProvider::new(OxrProviderConfig {
        endpoint: Some("http://127.0.0.1:8000/v1".into()),
        model_identity: Some("OXR-1.0".into()),
        ..OxrProviderConfig::default()
    });
    match provider.availability().await {
        ProviderAvailability::Unavailable { reason } => {
            assert!(reason.contains("credential"), "{reason}");
        }
        ProviderAvailability::Ready => panic!("a missing credential is never ready"),
    }
}

#[test]
fn select_provider_never_substitutes() {
    let oxr = unbound_provider();
    // A request for a different closure finds no provider.
    assert!(select_provider("docling/9.9", std::slice::from_ref(&oxr)).is_none());
    // The named closure yields the named provider, even unprovisioned.
    assert!(select_provider(oxr.closure_id(), std::slice::from_ref(&oxr)).is_some());
}

#[test]
fn engine_configuration_rejects_undeclared_fields() {
    let remote: OxrEngineBinding = serde_json::from_str(r#"{"kind":"remote"}"#).unwrap();
    assert_eq!(remote, OxrEngineBinding::Remote {});
    assert!(serde_json::from_str::<OxrEngineBinding>(r#"{"kind":"remote","attestation":"not a remote field"}"#).is_err());
    let local: OxrEngineBinding = serde_json::from_str(r#"{"kind":"local","attestation":null}"#).unwrap();
    assert_eq!(local, OxrEngineBinding::Local { attestation: None });
    assert!(serde_json::from_str::<OxrEngineBinding>(r#"{"kind":"local","unexpected":true}"#).is_err());
}
