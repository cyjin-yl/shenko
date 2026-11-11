//! docs/specs/cognition-units.md#spec-51-4 append-node rollback: the verified mechanics of
//! [`shenko_update::rollback::append_rollback`].
//!
//! These tests exercise the verification contract of the append-node path
//! through a narrow mock of [`RootChain`]. They do NOT test Store migration
//! (that is `rollback_migration.rs`) and do NOT test the CLI surface. Each
//! test names the contract step it exercises from `append_rollback`'s
//! documented ordering.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Mutex;

use shenko_store::Store;
use shenko_types::ids::EntityId;
use shenko_types::object::Digest;

use shenko_update::rollback::{
    append_rollback, parse_node_digest, AppendedNode, ExternalEffectReconciliation,
    RollbackError, RollbackRequest, RootChain, RootComposition, StateClass, UnretractedEffect,
};

// ---------------------------------------------------------------------------
// Test helpers
// ---------------------------------------------------------------------------

fn test_store() -> (Store, PathBuf) {
    let dir = std::env::temp_dir().join(format!("shenko-rb-{}", EntityId::new()));
    let cas = dir.join("cas");
    std::fs::create_dir_all(&cas).unwrap();
    let db = dir.join("shenko.db");
    let store = Store::open(db, cas).unwrap();
    (store, dir)
}

fn digest_of(text: &str) -> Digest {
    Digest::blake3(text.as_bytes())
}

/// A deterministic mock chain that holds at most two nodes.
struct MockChain {
    agent: EntityId,
    head: Mutex<Option<Digest>>,
    nodes: Mutex<Vec<RootComposition>>,
    /// When set, `append_rollback_node` returns this instead of appending.
    append_override: Mutex<Option<AppendedNode>>,
}

impl MockChain {
    fn new(agent: EntityId) -> Self {
        Self {
            agent,
            head: Mutex::new(None),
            nodes: Mutex::new(Vec::new()),
            append_override: Mutex::new(None),
        }
    }

    fn add_node(&self, composition: RootComposition) {
        let mut head = self.head.lock().unwrap();
        *head = Some(composition.node.clone());
        self.nodes.lock().unwrap().push(composition);
    }

    fn set_append_override(&self, node: AppendedNode) {
        *self.append_override.lock().unwrap() = Some(node);
    }
}

impl RootChain for MockChain {
    fn agent_id(&self) -> anyhow::Result<EntityId> {
        Ok(self.agent)
    }

    fn head(&self) -> anyhow::Result<Option<Digest>> {
        Ok(self.head.lock().unwrap().clone())
    }

    fn composition(&self, node: &Digest) -> anyhow::Result<Option<RootComposition>> {
        Ok(self
            .nodes
            .lock()
            .unwrap()
            .iter()
            .find(|c| &c.node == node)
            .cloned())
    }

    fn append_rollback_node(&self, target: &RootComposition) -> anyhow::Result<AppendedNode> {
        if let Some(ovr) = self.append_override.lock().unwrap().take() {
            let mut head = self.head.lock().unwrap();
            *head = Some(ovr.node.clone());
            return Ok(ovr);
        }
        // Normal mock: append a new node with the same composition.
        let new_node = Digest::blake3(
            format!("appended-for-{}", target.node).as_bytes(),
        );
        let comp = RootComposition {
            node: new_node.clone(),
            agent_id: target.agent_id,
            classes: target.classes.clone(),
            objects: target.objects.clone(),
        };
        self.nodes.lock().unwrap().push(comp);
        let mut head = self.head.lock().unwrap();
        *head = Some(new_node.clone());
        Ok(AppendedNode {
            node: new_node,
            reused_objects: target.objects.len() as u64,
        })
    }
}

fn full_classes() -> BTreeSet<StateClass> {
    StateClass::MANDATORY.iter().copied().collect()
}

fn partial_classes() -> BTreeSet<StateClass> {
    let mut set = BTreeSet::new();
    set.insert(StateClass::HomeFiles);
    set.insert(StateClass::SoftwareSlots);
    set
}

fn no_effects() -> ExternalEffectReconciliation {
    ExternalEffectReconciliation::Observed {
        items: Vec::new(),
    }
}

/// Extract the refusal message from a `RollbackError::Refused`, if any.
fn refused_msg(result: &Result<shenko_update::rollback::RollbackOutcome, RollbackError>) -> Option<&str> {
    match result {
        Err(RollbackError::Refused(msg)) => Some(msg.as_str()),
        _ => None,
    }
}

/// Register an agent in the store so `get_existing_agent_id` returns it.
fn register_agent(store: &Store, agent: EntityId) {
    // Use the Store's own API for creating a subject. The simplest way is
    // to admit a memory intent which implicitly registers the agent.
    let intent = EntityId::new();
    let record = shenko_store::MemoryIntentRecord::accepted(
        intent, agent, "save", "personal", "bootstrap", None, None,
    );
    store.admit_memory_intent(&record, None).unwrap();
}

/// Seed CAS objects so `object_exists` / `object_size` return true.
fn seed_cas_objects(store: &Store, objects: &[Digest]) {
    for obj in objects {
        // Write a tiny payload using the object's digest text as content
        // so the CAS holds it.
        let content = obj.0.as_bytes();
        let _ = store.cas.seal_object(
            content,
            "application/octet-stream",
            "test",
            "test-object",
            1,
        );
    }
}

// ---------------------------------------------------------------------------
// parse_node_digest
// ---------------------------------------------------------------------------

#[test]
fn parse_node_digest_accepts_canonical_blake3() {
    let data = b"hello rollback";
    let digest = Digest::blake3(data);
    let parsed = parse_node_digest(&digest.0).unwrap();
    assert_eq!(parsed, digest);
}

#[test]
fn parse_node_digest_rejects_non_canonical() {
    let result = parse_node_digest("sha256:abcd");
    assert!(matches!(result, Err(RollbackError::Refused(_))));

    let result = parse_node_digest("blake3:ABCD0000000000000000000000000000000000000000000000000000000000FF");
    assert!(matches!(result, Err(RollbackError::Refused(_))));

    let result = parse_node_digest("");
    assert!(matches!(result, Err(RollbackError::Refused(_))));
}

// ---------------------------------------------------------------------------
// Step 1: subject ownership
// ---------------------------------------------------------------------------

#[test]
fn rollback_refuses_when_no_subject_exists() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    let chain = MockChain::new(agent);
    let target = digest_of("target");
    let request = RollbackRequest {
        target,
        expected_head: None,
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("no local subject")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rollback_refuses_when_chain_subject_differs() {
    let (store, dir) = test_store();
    let agent_a = EntityId::new();
    let agent_b = EntityId::new();
    register_agent(&store, agent_a);
    let chain = MockChain::new(agent_b); // different agent
    let target = digest_of("target");
    let request = RollbackRequest {
        target,
        expected_head: None,
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("root-subject-mismatch")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Step 2: head comparison
// ---------------------------------------------------------------------------

#[test]
fn rollback_refuses_when_chain_head_moved() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);
    let current_head = digest_of("current-head");
    chain.add_node(RootComposition {
        node: current_head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });
    // Selection was made when the chain had no head, but now it does.
    let request = RollbackRequest {
        target: digest_of("some-target"),
        expected_head: None,
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("chain-moved")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Step 3: target existence and object presence
// ---------------------------------------------------------------------------

#[test]
fn rollback_refuses_unknown_target_node() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);
    let head = digest_of("head-node");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });
    // Target node does not exist in the chain.
    let request = RollbackRequest {
        target: digest_of("nonexistent-node"),
        expected_head: Some(head),
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("unknown-root-node")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rollback_refuses_when_cas_objects_missing() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let obj1 = digest_of("object-that-exists");
    let obj2 = Digest("blake3:0000000000000000000000000000000000000000000000000000000000000001".into());

    let head = digest_of("head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let target = digest_of("old-node");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![obj1.clone(), obj2.clone()],
    });

    // Only seed obj1 in CAS, not obj2.
    seed_cas_objects(&store, &[obj1]);

    let request = RollbackRequest {
        target,
        expected_head: Some(head),
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("root-objects-missing")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Step 5: composition equality
// ---------------------------------------------------------------------------

#[test]
fn rollback_refuses_when_appended_node_classes_differ() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let head = digest_of("head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let target = digest_of("old-node");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    // Override: the appended node claims only partial classes.
    let bad_node = digest_of("bad-appended");
    chain.set_append_override(AppendedNode {
        node: bad_node.clone(),
        reused_objects: 0,
    });
    // Pre-register the bad node's composition with wrong classes.
    chain.add_node(RootComposition {
        node: bad_node.clone(),
        agent_id: agent,
        classes: partial_classes(),
        objects: vec![],
    });

    let request = RollbackRequest {
        target,
        expected_head: Some(head),
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("append-content-mismatch")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Step 4: append returns a new node, not the current head
// ---------------------------------------------------------------------------

#[test]
fn rollback_refuses_when_append_returns_current_head() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let head = digest_of("head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let target = digest_of("old-node");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    // The append "returns" the current head: a rewind, not a growth.
    chain.set_append_override(AppendedNode {
        node: head.clone(),
        reused_objects: 0,
    });

    let request = RollbackRequest {
        target,
        expected_head: Some(head),
        activation: None,
    };
    let result = append_rollback(&store, &chain, &request, no_effects());
    assert!(refused_msg(&result).is_some_and(|msg| msg.contains("chain-rewound")), "{result:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Happy path: full-state rollback
// ---------------------------------------------------------------------------

#[test]
fn successful_full_state_rollback() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let obj1 = digest_of("home-files-content");
    let obj2 = digest_of("wasm-memory-content");
    seed_cas_objects(&store, &[obj1.clone(), obj2.clone()]);

    let head = digest_of("current-head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![obj1.clone(), obj2.clone()],
    });

    let target = digest_of("historical-node");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![obj1.clone(), obj2.clone()],
    });

    let request = RollbackRequest {
        target: target.clone(),
        expected_head: Some(head.clone()),
        activation: Some(EntityId::new()),
    };
    let outcome = append_rollback(&store, &chain, &request, no_effects()).unwrap();

    assert_eq!(outcome.target, target);
    assert_ne!(outcome.new_node, head, "new node is not the old head");
    assert_ne!(outcome.new_node, target, "new node is not the target itself");
    assert_eq!(outcome.head_before, Some(head));
    assert_eq!(outcome.head_after, outcome.new_node);
    assert_eq!(outcome.agent_id, agent);
    assert_eq!(outcome.classes, full_classes());
    assert!(outcome.missing_classes.is_empty());
    assert!(outcome.full_state, "all mandatory classes present");
    assert_eq!(outcome.reused_objects, 2);
    assert!(outcome.reused_bytes > 0);
    assert_eq!(outcome.schema_version, 1);
    assert!(outcome.activation.is_some());
    assert!(outcome.created_at_ms > 0);
    // The receipt is a real sealed CAS object.
    assert!(store.cas.object_exists(&outcome.receipt.digest));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// Partial state: missing classes reported, full_state = false
// ---------------------------------------------------------------------------

#[test]
fn partial_state_rollback_reports_missing_classes() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let head = digest_of("current-head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: partial_classes(),
        objects: vec![],
    });

    let target = digest_of("old-partial-node");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: partial_classes(),
        objects: vec![],
    });

    let request = RollbackRequest {
        target: target.clone(),
        expected_head: Some(head),
        activation: None,
    };
    let outcome = append_rollback(&store, &chain, &request, no_effects()).unwrap();

    assert!(!outcome.full_state, "partial classes → not full_state");
    assert!(!outcome.missing_classes.is_empty());
    assert!(outcome.missing_classes.contains(&StateClass::WasmLinearMemory));
    assert!(outcome.missing_classes.contains(&StateClass::ExecutionStack));
    assert!(outcome.missing_classes.contains(&StateClass::InterpreterObjectGraph));
    assert!(outcome.missing_classes.contains(&StateClass::CognitionSelectors));
    assert!(!outcome.missing_classes.contains(&StateClass::HomeFiles));
    assert!(!outcome.missing_classes.contains(&StateClass::SoftwareSlots));

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// External effects: observed and unobserved
// ---------------------------------------------------------------------------

#[test]
fn rollback_records_external_effects_in_outcome() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let head = digest_of("head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let target = digest_of("target");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let effects = ExternalEffectReconciliation::Observed {
        items: vec![
            UnretractedEffect {
                mechanism: "outbox".into(),
                record: "msg-001".into(),
                effect: "sent".into(),
            },
            UnretractedEffect {
                mechanism: "webhook".into(),
                record: "hook-002".into(),
                effect: "unknown".into(),
            },
        ],
    };

    let request = RollbackRequest {
        target: target.clone(),
        expected_head: Some(head),
        activation: None,
    };
    let outcome = append_rollback(&store, &chain, &request, effects).unwrap();

    assert_eq!(outcome.external_effects.observed_len(), 2);
    match &outcome.external_effects {
        ExternalEffectReconciliation::Observed { items } => {
            assert_eq!(items[0].mechanism, "outbox");
            assert_eq!(items[1].effect, "unknown");
        }
        _ => panic!("expected Observed"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rollback_with_unobserved_effects_records_the_gap() {
    let (store, dir) = test_store();
    let agent = EntityId::new();
    register_agent(&store, agent);
    let chain = MockChain::new(agent);

    let head = digest_of("head");
    chain.add_node(RootComposition {
        node: head.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let target = digest_of("target");
    chain.add_node(RootComposition {
        node: target.clone(),
        agent_id: agent,
        classes: full_classes(),
        objects: vec![],
    });

    let effects = ExternalEffectReconciliation::Unobserved {
        detail: "no effect ledger installed".into(),
    };
    let request = RollbackRequest {
        target: target.clone(),
        expected_head: Some(head),
        activation: None,
    };
    let outcome = append_rollback(&store, &chain, &request, effects).unwrap();

    assert_eq!(outcome.external_effects.observed_len(), 0);
    match &outcome.external_effects {
        ExternalEffectReconciliation::Unobserved { detail } => {
            assert!(detail.contains("no effect ledger"));
        }
        _ => panic!("expected Unobserved"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// StateClass invariants
// ---------------------------------------------------------------------------

#[test]
fn state_class_mandatory_covers_all_six() {
    assert_eq!(StateClass::MANDATORY.len(), 6);
    let set: BTreeSet<StateClass> = StateClass::MANDATORY.iter().copied().collect();
    assert!(set.contains(&StateClass::HomeFiles));
    assert!(set.contains(&StateClass::SoftwareSlots));
    assert!(set.contains(&StateClass::CognitionSelectors));
    assert!(set.contains(&StateClass::WasmLinearMemory));
    assert!(set.contains(&StateClass::ExecutionStack));
    assert!(set.contains(&StateClass::InterpreterObjectGraph));
}

#[test]
fn state_class_as_str_roundtrips_with_serde() {
    for class in &StateClass::MANDATORY {
        let json = serde_json::to_string(class).unwrap();
        let back: StateClass = serde_json::from_str(&json).unwrap();
        assert_eq!(*class, back);
        // The wire name matches as_str.
        assert!(json.contains(class.as_str()));
    }
}
