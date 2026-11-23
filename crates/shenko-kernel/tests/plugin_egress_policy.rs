//! Root egress policy: undeclared, declared and violating attempts.
//!
//! These tests exercise the real Cordis root, the real generation lifetime and
//! the one destination evaluator. They make no network attempt and no model
//! call: an admission is a decision, and the point under test is that only a
//! live declaration for the exact plugin, capability, protocol, host and port
//! produces one.
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use cordis_core::{Context, Plugin};
use shenko_kernel::policy::{PluginPolicyRegistry, PolicyRegistrationError};
use shenko_kernel::Supervisor;
use shenko_types::egress::{
    evaluate_egress, EgressDeclaration, EgressDeclarationError, EgressDenialCode, EgressDecision,
    EgressPortSpan, EgressProtocol, EgressRequest, EgressScope, HostPattern,
};
use shenko_types::ids::{EntityId, RuntimeGeneration};
use shenko_types::object::Digest;

const DECLARING: &str = "00000000-0000-0000-0000-0000000000e1";
const UNDECLARED: &str = "00000000-0000-0000-0000-0000000000e2";
const CAPABILITY: &str = "native.example.fetch";

fn plugin(id: &str) -> EntityId {
    id.parse().expect("test plugin identity")
}

fn declaration(plugin_id: EntityId) -> EgressDeclaration {
    EgressDeclaration {
        plugin_id,
        capability_id: CAPABILITY.into(),
        capability_version: "1".into(),
        purpose: "fetch one declared reference from the documentation host".into(),
        scopes: vec![EgressScope {
            protocol: EgressProtocol::Https,
            host: HostPattern::SubdomainsOf {
                domain: "example.com".into(),
            },
            ports: vec![EgressPortSpan::single(443)],
        }],
    }
}

fn request(
    plugin_id: EntityId,
    protocol: EgressProtocol,
    host: &str,
    port: u16,
) -> EgressRequest {
    EgressRequest::new(
        plugin_id,
        CAPABILITY,
        protocol,
        host,
        port,
        "read one declared reference",
    )
}

/// A real Cordis plugin that declares its egress in `apply` and gives the
/// generation's teardown the matching withdrawal.
struct Declaring {
    declaration: EgressDeclaration,
    authority: Arc<Mutex<Option<shenko_kernel::policy::EgressAuthority>>>,
}

impl Plugin for Declaring {
    type Config = ();
    type Input = ();
    type PrepareError = Infallible;
    type ApplyError = std::io::Error;

    fn prepare(&self, (): ()) -> Result<(), Infallible> {
        Ok(())
    }

    async fn apply(&self, ctx: Context, (): &()) -> Result<(), std::io::Error> {
        let policy = ctx
            .try_service::<PluginPolicyRegistry>()
            .map_err(std::io::Error::other)?;
        let handle = policy
            .declare(self.declaration.clone())
            .map_err(std::io::Error::other)?;
        *self.authority.lock().expect("fixture authority lock") = Some(handle.authority());
        let teardown = Arc::clone(&policy);
        ctx.effect_sync(move || {
            teardown.withdraw_declaration(&handle);
        })
        .map_err(std::io::Error::other)?;
        Ok(())
    }
}

#[test]
fn the_one_evaluator_refuses_undeclared_and_undescribed_attempts() {
    let declaring = declaration(plugin(DECLARING));

    // No declaration at all: deny by default.
    let undeclared = evaluate_egress(
        &[],
        &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
    );
    assert_eq!(
        undeclared.denial_code(),
        Some(EgressDenialCode::UndeclaredPlugin)
    );

    // The plugin declared other capabilities, but not this one.
    let other_capability = EgressRequest::new(
        plugin(DECLARING),
        "native.example.other",
        EgressProtocol::Https,
        "docs.example.com",
        443,
        "read one declared reference",
    );
    assert_eq!(
        evaluate_egress(std::slice::from_ref(&declaring), &other_capability).denial_code(),
        Some(EgressDenialCode::UndeclaredCapability)
    );

    // The declared scope admits the exact protocol, host and port, including
    // case and root-dot spellings of the same name.
    for host in ["docs.example.com", "Docs.Example.COM", "api.docs.example.com."] {
        let decision = evaluate_egress(
            std::slice::from_ref(&declaring),
            &request(plugin(DECLARING), EgressProtocol::Https, host, 443),
        );
        match decision {
            EgressDecision::Allowed { matched } => {
                assert_eq!(matched.capability_id, CAPABILITY);
                assert_eq!(matched.scope_index, 0);
                assert_eq!(matched.declaration_digest, declaring.digest());
            }
            other => panic!("{host} should be admitted, got {other:?}"),
        }
    }

    // Every dimension of the declaration is checked separately, so a refusal
    // says which one the attempt violated.
    let violations = [
        (
            request(plugin(DECLARING), EgressProtocol::Tcp, "docs.example.com", 443),
            EgressDenialCode::ProtocolNotDeclared,
        ),
        (
            request(plugin(DECLARING), EgressProtocol::Https, "notexample.com", 443),
            EgressDenialCode::HostNotDeclared,
        ),
        (
            request(plugin(DECLARING), EgressProtocol::Https, "evil-example.com", 443),
            EgressDenialCode::HostNotDeclared,
        ),
        (
            request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 8443),
            EgressDenialCode::PortNotDeclared,
        ),
    ];
    for (violating, expected) in violations {
        assert_eq!(
            evaluate_egress(std::slice::from_ref(&declaring), &violating).denial_code(),
            Some(expected),
            "{violating:?} must be refused as {expected}"
        );
    }

    // A request that cannot be canonicalised is refused instead of being
    // compared loosely.
    for invalid in [
        request(plugin(DECLARING), EgressProtocol::Https, "", 443),
        request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com:443", 443),
        request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com/x", 443),
        request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 0),
    ] {
        assert_eq!(
            evaluate_egress(std::slice::from_ref(&declaring), &invalid).denial_code(),
            Some(EgressDenialCode::InvalidRequest),
            "{invalid:?} must be refused as invalid"
        );
    }
}

#[test]
fn declarations_must_state_a_purpose_a_scope_and_a_usable_port() {
    let mut no_purpose = declaration(plugin(DECLARING));
    no_purpose.purpose = "  ".into();
    assert_eq!(
        no_purpose.validate(),
        Err(EgressDeclarationError::EmptyPurpose)
    );

    let mut no_scope = declaration(plugin(DECLARING));
    no_scope.scopes.clear();
    assert_eq!(no_scope.validate(), Err(EgressDeclarationError::EmptyScope));

    let mut zero_port = declaration(plugin(DECLARING));
    zero_port.scopes[0].ports = vec![EgressPortSpan::single(0)];
    assert_eq!(
        zero_port.validate(),
        Err(EgressDeclarationError::ZeroPort { scope: 0 })
    );

    let mut overlapping = declaration(plugin(DECLARING));
    overlapping.scopes[0].ports = vec![
        EgressPortSpan { first: 443, last: 8443 },
        EgressPortSpan { first: 8443, last: 9000 },
    ];
    assert_eq!(
        overlapping.validate(),
        Err(EgressDeclarationError::OverlappingPorts { scope: 0 })
    );

    let policy = PluginPolicyRegistry::new();
    assert_eq!(
        policy.declare(no_purpose),
        Err(PolicyRegistrationError::InvalidDeclaration(
            EgressDeclarationError::EmptyPurpose
        ))
    );
    assert!(policy.declarations().is_empty());
    assert_eq!(policy.policy_revision(), 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_root_registry_admits_only_the_live_declared_scope() {
    let supervisor = Supervisor::new(RuntimeGeneration::new("egress-policy-registry"));
    supervisor
        .publish_services()
        .await
        .expect("publish the root services");
    let policy = supervisor
        .context()
        .try_service::<PluginPolicyRegistry>()
        .expect("the root policy is a published service");
    assert!(Arc::ptr_eq(&policy, &supervisor.policy));

    let handle = policy
        .declare(declaration(plugin(DECLARING)))
        .expect("a complete declaration is admitted");
    let authority = handle.authority();
    assert_eq!(policy.policy_revision(), 1);

    let allowed = policy
        .admit(
            &authority,
            &request(plugin(DECLARING), EgressProtocol::Https, "Docs.Example.COM.", 443),
        )
        .expect("the declared scope admits the attempt");
    assert!(allowed.receipt().is_allowed());
    assert_eq!(allowed.receipt().request.host, "docs.example.com");
    assert_eq!(
        allowed.matched().expect("an admission names its scope").scope_index,
        0
    );

    // A destination outside the declaration is refused, and the refusal names
    // the dimension it violated.
    let violating = policy
        .admit(
            &authority,
            &request(plugin(DECLARING), EgressProtocol::Https, "notexample.com", 443),
        )
        .expect_err("a host outside the declared domain is refused");
    assert_eq!(violating.code(), EgressDenialCode::HostNotDeclared);

    // One plugin cannot spend another plugin's declaration by naming it in the
    // request: the authority is bound to the plugin that declared it.
    let borrowed = policy
        .admit(
            &authority,
            &request(plugin(UNDECLARED), EgressProtocol::Https, "docs.example.com", 443),
        )
        .expect_err("another plugin's identity is refused");
    assert_eq!(borrowed.code(), EgressDenialCode::AuthorityMismatch);

    // Replacing the declaration makes the previous authority stale: the new
    // bytes are a different permission, not a re-read of the old one.
    let replacement = policy
        .declare(declaration(plugin(DECLARING)))
        .expect("the upgraded declaration replaces the old one");
    let stale = policy
        .admit(
            &authority,
            &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
        )
        .expect_err("a replaced registration epoch is stale");
    assert_eq!(stale.code(), EgressDenialCode::AuthorityStale);
    assert!(
        policy
            .admit(
                &replacement.authority(),
                &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
            )
            .is_ok()
    );

    // Withdrawal ends admission entirely; the old authority does not fall back
    // to anything broader.
    assert!(policy.withdraw_declaration(&replacement));
    let withdrawn = policy
        .admit(
            &replacement.authority(),
            &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
        )
        .expect_err("a withdrawn declaration admits nothing");
    assert_eq!(withdrawn.code(), EgressDenialCode::AuthorityStale);
    assert!(policy.declaration(plugin(DECLARING), CAPABILITY).is_none());

    // Every attempt produced a receipt, allowed or refused.
    let receipts = policy.recent_receipts();
    assert_eq!(receipts.len(), 6);
    assert!(receipts.iter().any(|receipt| receipt.is_allowed()));
    assert!(receipts.iter().any(|receipt| {
        receipt.denial_code() == Some(EgressDenialCode::HostNotDeclared)
    }));
    let last = receipts.last().expect("the withdrawal attempt is recorded");
    assert_eq!(last.digest(), Digest::blake3(&last.canonical_bytes()));
    assert_eq!(policy.dropped_receipts(), 0);

    supervisor.stop().await.expect("stop the host");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_installed_generation_declares_in_apply_and_loses_admission_on_uninstall() {
    let supervisor = Supervisor::new(RuntimeGeneration::new("egress-policy-generation"));
    supervisor
        .publish_services()
        .await
        .expect("publish the root services");
    let policy = supervisor
        .context()
        .try_service::<PluginPolicyRegistry>()
        .expect("the root policy is a published service");
    let authority = Arc::new(Mutex::new(None));
    let fiber = supervisor
        .spawn(
            Declaring {
                declaration: declaration(plugin(DECLARING)),
                authority: Arc::clone(&authority),
            },
            (),
        )
        .await
        .expect("install the declaring generation");
    let authority = authority
        .lock()
        .expect("fixture authority lock")
        .clone()
        .expect("apply registered the declaration and handed back its authority");
    assert!(policy.declaration(plugin(DECLARING), CAPABILITY).is_some());
    assert!(
        policy
            .admit(
                &authority,
                &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
            )
            .is_ok()
    );

    supervisor
        .uninstall(&fiber)
        .await
        .expect("uninstall the declaring generation");
    assert!(
        policy.declaration(plugin(DECLARING), CAPABILITY).is_none(),
        "the generation teardown withdraws the declaration"
    );
    let denial = policy
        .admit(
            &authority,
            &request(plugin(DECLARING), EgressProtocol::Https, "docs.example.com", 443),
        )
        .expect_err("a retired generation cannot keep egressing");
    assert_eq!(denial.code(), EgressDenialCode::AuthorityStale);

    supervisor.stop().await.expect("stop the host");
}
