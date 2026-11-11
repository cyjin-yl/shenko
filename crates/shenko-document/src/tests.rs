//! Structural tests for the document.parse boundary. These prove the
//! contract shapes and the honest-Unavailable behavior — they are NOT a real
//! OXR parse (which needs GPU + model weights + the engine, per issue #89).

use super::*;

fn request(pages: &[u32]) -> ParseRequest {
    ParseRequest {
        schema_version: DOCUMENT_PARSE_DEFINITION_VERSION,
        source_digest: "blake3:deadbeef".into(),
        source_name: Some("doc.pdf".into()),
        pages: pages.iter().map(|i| PageRef { page_index: *i }).collect(),
        closure_id: "oxr/1.0".into(),
        scope: Some("scope-a".into()),
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
    other_scope.scope = Some("scope-b".into());
    assert_ne!(k0, page_cache_key(&other_scope, PageRef { page_index: 0 }));
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
    let run = run_parse(
        &request(&[0, 1]),
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
    assert!(select_provider("oxr/1.0", &[oxr]).is_some());
}
