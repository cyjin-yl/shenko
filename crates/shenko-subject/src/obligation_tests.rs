//! Real parser, Store/CAS and S0/S1 consumers. No provider efficacy claim.
use super::*;
use super::super as runner;
use shenko_types::output::{OutputAction, ParsedOutput};
use shenko_store::cognition::{CognitionImportOptions, CognitionSnapshotOptions};

fn terminal(id: EntityId, state: &str) -> String {
    format!("[obligation:terminal] {}", serde_json::json!({
        "obligation_id":id,"state":state,"reason":"the authored terminal judgment"}))
}

fn parse(store: &Store, subject: &SubjectLoopState, raw: &str) -> Result<ParsedOutput, ShenkoError> {
    let raw_ref = store.cas.seal_object(raw.as_bytes(), "text/plain", "model_results", "text", 1).unwrap().object;
    crate::output::parse_output(subject.step.step_id, raw_ref, raw,
        &subject.identity_service, &subject.channels, None)
}

fn obligation_row(dir: &std::path::Path, id: EntityId) -> Vec<String> {
    rusqlite::Connection::open(dir.join("store.db")).unwrap().query_row(
        "SELECT obligation_id,agent_id,step_id,COALESCE(origin_event_id,''),description,
            COALESCE(depends_on_task_id,''),resume_condition,COALESCE(due_at,''),decision_ref,created_seq,state,updated_seq
         FROM subject_obligations WHERE obligation_id=?1", [id.to_string()],
        |row| (0..12).map(|index| if index == 9 || index == 11 {
            row.get::<_, i64>(index).map(|number| number.to_string())
        } else { row.get(index) }).collect(),
    ).unwrap()
}

fn terminal_receipts(store: &Store) -> Vec<ObligationTerminalReceipt> {
    store.committed_intents(Some("policy"), None, 1024).unwrap().into_iter()
        .filter(|intent| intent.contract == "subject/obligation-terminal/1")
        .map(|intent| {
            assert_eq!(intent.state, "settled");
            serde_json::from_str(&intent.result_ref.unwrap()).unwrap()
        }).collect()
}

async fn seal(subject: &mut SubjectLoopState) {
    let cancel = tokio_util::sync::CancellationToken::new();
    cancel.cancel();
    execute_step(subject.step.step_no, &[], subject, &cancel).await.unwrap();
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
}

fn rewrite_decision(store: &Store, step: &mut SubjectStep, edit: impl FnOnce(&mut StepDecision)) {
    let mut decision: StepDecision = serde_json::from_slice(&store.cas.read_object(
        &step.decision_ref.as_ref().unwrap().digest).unwrap()).unwrap();
    edit(&mut decision);
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", decision.schema_version).unwrap().object);
}

/// Build an S1 candidate from the real parser, so tests can corrupt only the
/// binding being exercised rather than replacing Store or settlement logic.
fn candidate(store: &Store, subject: &SubjectLoopState, raw: &str, next: EntityId) -> SubjectStep {
    let output = parse(store, subject, raw).unwrap();
    let mut decision = StepDecision {
        schema_version:8, step_id:subject.step.step_id, output,
        identity_intent:None, input_plan_ref:subject.step.input_plan_ref.clone().unwrap(),
        model_result_ref:subject.step.model_result_ref.clone(), applied_steering_refs:vec![],
        event_dispositions:vec![], delegated_task_intents:vec![], child_dialogue_intents:vec![],
        memory_intents:vec![], policy_intents:vec![], publication_intents:vec![],
        pending_obligations:vec![], attention_policy:None, evidence_refs:vec![], successor_step_id:next,
        unknown_acknowledgements:vec![],
    };
    for control in &decision.output.controls {
        match &control.action {
            OutputAction::ObligationTerminal { change } => decision.policy_intents.push(PolicyIntent {
                intent_id:control.action_id, kind:PolicyKind::ObligationTerminal { change:change.clone() },
                description:"subject obligation terminal change".into(), target_revision:None,
            }),
            OutputAction::Obligation { spec } => decision.pending_obligations.push(PendingObligation {
                obligation_id:control.action_id, description:spec.description.clone(),
                depends_on_task_id:spec.depends_on_task_id, resume_condition:spec.resume_condition.clone().unwrap_or_default(),
                origin_event_id:spec.origin_event_id, due_at:spec.due_at,
            }),
            _ => {},
        }
    }
    let mut step = subject.step.clone();
    step.state = SubjectStepState::Committed;
    step.record_revision = RecordRevision(step.record_revision.0 + 1);
    step.decision_ref = Some(store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", 8).unwrap().object);
    step
}


#[tokio::test]
async fn obligations_reach_model_and_retire_with_exact_creation_provenance() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    // An obligation origin is a durable input event: the fixture admits it so
    // the exported archive carries the event its promise answers.
    let origin = admit(&store, agent, &message("a promise that needs a later judgment"));
    // The parser is strict: unknown state, nil/invalid id, blank reason,
    // unknown fields and duplicate terminals in one output are all rejected.
    let id = EntityId::new();
    assert!(parse(&store, &subject, &format!("[obligation:terminal] {}", serde_json::json!({"obligation_id":id,"state":"open","reason":"no"}))).is_err());
    assert!(parse(&store, &subject, &format!("[obligation:terminal] {}", serde_json::json!({"obligation_id":id,"state":"completed","reason":"done","extra":true}))).is_err());
    assert!(parse(&store, &subject, &format!("{}\n{}", terminal(id,"completed"), terminal(id,"cancelled"))).is_err());
    let raw = format!("[obligation] {}\n[obligation] {}\nI keep both promises.",
        serde_json::json!({"description":"finish the observation","origin_event_id":origin.event_id,
            "resume_condition":"after the result arrives","due_at":"2030-01-02T03:04:05Z"}),
        serde_json::json!({"description":"a separately cancellable promise","resume_condition":"owner judgment"}));
    let created = commit_output(&store, &mut subject, 1, &[origin], &raw).await;
    let open = store.open_subject_obligations(agent).unwrap();
    assert_eq!(open.len(), 2);
    let before: Vec<_> = open.iter().map(|item| obligation_row(&dir, item.obligation_id)).collect();
    let profile = crate::context_projection::prepare_context(&mut subject, &[]).unwrap().unwrap().context.profile_digest;
    seal(&mut subject).await;
    let input_ref = subject.step.input_plan_ref.clone().unwrap();
    let input_bytes = store.cas.read_object(&input_ref.digest).unwrap();
    let frozen = runner::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(frozen.obligations, open);
    assert!(frozen.payload.system_prompt.contains(&serde_json::to_string(&open).unwrap()));
    assert_eq!(frozen.context.profile_digest, profile);
    let payload = serde_json::to_vec(&frozen.payload).unwrap();
    drop((subject, store));
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(store.clone(), agent).unwrap();
    let reopened = runner::frozen_input(&store, &subject.step).unwrap();
    assert_eq!(reopened.obligations, open);
    assert_eq!(store.cas.read_object(&input_ref.digest).unwrap(), input_bytes);
    assert_eq!(serde_json::to_vec(&reopened.payload).unwrap(), payload);
    let raw = format!("{}\n{}\n[remember] another legitimate S1 action\nI made both judgments.",
        terminal(open[0].obligation_id,"completed"), terminal(open[1].obligation_id,"cancelled"));
    let original_head = subject.head.clone();
    let retired_step = record_result(&store, &mut subject, &raw);
    execute_step(2, &[], &mut subject, &tokio_util::sync::CancellationToken::new()).await.unwrap().unwrap();
    assert!(store.open_subject_obligations(agent).unwrap().is_empty());
    for (index, item) in open.iter().enumerate() {
        let after = obligation_row(&dir, item.obligation_id);
        assert_eq!(&before[index][..10], &after[..10], "all creation fields are immutable");
        assert_eq!(after[2], created.to_string());
        assert_eq!(after[10], if index == 0 { "completed" } else { "cancelled" });
        assert!(after[11].parse::<u64>().unwrap() > after[9].parse::<u64>().unwrap());
    }
    let receipts = terminal_receipts(&store);
    assert_eq!(receipts.len(),2);
    assert!(receipts.iter().all(|receipt| receipt.step_id == retired_step
        && receipt.input_plan_ref == input_ref && receipt.outcome == ObligationTerminalOutcome::Applied
        && open.contains(&receipt.sealed_obligation)));
    let committed = store.get_step(&subject.head.branch_id, retired_step).unwrap().unwrap();
    assert!(store.commit_step_and_advance_head(&committed,&original_head,subject.step.step_id).is_err());
    store.start_subject_dispatcher().unwrap();
    store.start_subject_dispatcher().unwrap();
    assert_eq!(terminal_receipts(&store),receipts);
    assert_eq!(store.settle_subject_memory().unwrap(),1);
    let next = crate::context_projection::prepare_context(&mut subject,&[]).unwrap().unwrap();
    assert!(next.obligations.is_empty());
    assert!(!next.payload.system_prompt.contains(&open[0].obligation_id.to_string()));
    assert_eq!(next.context.profile_digest,profile,"volatile obligations do not rebind the profile");
    drop((subject,store));
    let reopened = Store::open(dir.join("store.db"),dir.join("cas")).unwrap();
    reopened.start_subject_dispatcher().unwrap();
    assert_eq!(terminal_receipts(&reopened),receipts);
    assert!(reopened.open_subject_obligations(agent).unwrap().is_empty());
}

#[tokio::test]
async fn obligation_s1_rejects_spoofed_unsealed_foreign_and_duplicate_changes() {
    for defect in ["policy_id","policy_change","missing_policy","missing_control","unsealed","foreign","body","duplicate","same_step"] {
        let (dir,store,agent)=fixture();
        let mut subject=state(store.clone(),agent).unwrap();
        commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"keep this promise\"}").await;
        let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
        if defect=="body" {
            let mut preparing=sealed_preparing_step(&mut subject);
            let mut input=runner::frozen_input(&store,&preparing).unwrap();
            input.obligations[0].description="forged snapshot body".into();
            preparing.input_plan_ref=Some(store.cas.seal_object(&serde_json::to_vec(&input).unwrap(),
                "application/json","input_plans","subject_step_input",12).unwrap().object);
            store.upsert_step(&preparing,&subject.head,RecordRevision(0)).unwrap();
            subject.step=preparing;
        } else { seal(&mut subject).await; }
        if defect=="foreign" {
            rusqlite::Connection::open(dir.join("store.db")).unwrap().execute(
                "UPDATE subject_obligations SET agent_id=?1",[EntityId::new().to_string()]).unwrap();
        }
        let target=if defect=="unsealed" { EntityId::new() } else { obligation.obligation_id };
        let raw=if defect=="same_step" {
            // Create a new obligation and terminal-target it in the same
            // output: the target won't exist in the sealed S0 yet.
            format!("[obligation] {{\"description\":\"new promise\"}}\n{}",terminal(EntityId::new(),"completed"))
        } else { terminal(target,"completed") };
        record_result(&store,&mut subject,&raw);
        let next=EntityId::new();
        let mut step=candidate(&store,&subject,&raw,next);
        rewrite_decision(&store,&mut step,|decision| match defect {
            "policy_id"=>decision.policy_intents[0].intent_id=EntityId::new(),
            "policy_change"=>if let PolicyKind::ObligationTerminal { change }=&mut decision.policy_intents[0].kind { change.reason="forged policy".into(); },
            "missing_policy"=>decision.policy_intents.clear(),
            "missing_control"=>decision.output.controls.clear(),
            "duplicate"=>{
                let mut control=decision.output.controls[0].clone(); control.action_id=EntityId::new();
                let mut policy=decision.policy_intents[0].clone(); policy.intent_id=control.action_id;
                decision.output.controls.push(control);decision.policy_intents.push(policy);
            },
            "same_step"=>{
                // Point the terminal at the obligation just created in this
                // same decision. It exists in pending_obligations but not in
                // the sealed S0 the request actually saw.
                let id=decision.pending_obligations[0].obligation_id;
                for control in &mut decision.output.controls {
                    if let OutputAction::ObligationTerminal { change }=&mut control.action { change.obligation_id=id; }
                }
                if let PolicyKind::ObligationTerminal { change }=&mut decision.policy_intents[0].kind { change.obligation_id=id; }
            },
            _=>{},
        });
        assert!(store.commit_step_and_advance_head(&step,&subject.head,next).is_err(),"{defect}");
        if defect!="same_step" {
            assert_eq!(store.load_subject(agent).unwrap().0.committed_step_no,1,"{defect}");
            assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"open");
        } else {
            // same_step creates a new obligation; the original stays open
            // and no new obligation is committed.
            assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"open");
            assert!(store.open_subject_obligations(agent).unwrap().iter().all(|item|item.obligation_id==obligation.obligation_id));
        }
        assert!(terminal_receipts(&store).is_empty());
    }
}

#[tokio::test]
async fn obligation_successor_conflict_rolls_back_projection_and_settlement() {
    let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    let created=commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"atomic promise\"}").await;
    let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
    let before=obligation_row(&dir,obligation.obligation_id);
    seal(&mut subject).await;let raw=terminal(obligation.obligation_id,"completed");
    record_result(&store,&mut subject,&raw);
    let step=candidate(&store,&subject,&raw,created);
    assert!(store.commit_step_and_advance_head(&step,&subject.head,created).is_err());
    assert_eq!(obligation_row(&dir,obligation.obligation_id),before);
    assert!(terminal_receipts(&store).is_empty());
    assert_eq!(store.load_subject(agent).unwrap().1.state,SubjectStepState::ResultRecorded);
    // A rejected S1 closes the external writer; reopen the Store before the
    // retry, exactly as a real subject recovery would.
    drop((subject,store));
    let store=Arc::new(Store::open(dir.join("store.db"),dir.join("cas")).unwrap());
    let mut subject=state(store.clone(),agent).unwrap();
    execute_step(2,&[],&mut subject,&tokio_util::sync::CancellationToken::new()).await.unwrap().unwrap();
    assert_eq!(terminal_receipts(&store).len(),1);
    assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"completed");
}

#[tokio::test]
async fn obligation_late_sealed_request_rejects_without_discarding_other_actions() {
    let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"a stale snapshot promise\"}").await;
    let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
    commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,"completed")).await;
    // Supply the retained open body as the actual next S0, using the same
    // renderer and Store seal admission. This isolates stale settlement;
    // it does not claim a physical concurrent-writer execution.
    let mut preparing=sealed_preparing_step(&mut subject);
    let mut input=runner::frozen_input(&store,&preparing).unwrap();
    input.obligations=vec![obligation.clone()];
    input.payload.system_prompt=runner::build_system_prompt(&subject,&[],&input.obligations);
    preparing.input_plan_ref=Some(store.cas.seal_object(&serde_json::to_vec(&input).unwrap(),
        "application/json","input_plans","subject_step_input",12).unwrap().object);
    store.upsert_step(&preparing,&subject.head,RecordRevision(0)).unwrap();subject.step=preparing;
    record_result(&store,&mut subject,&format!("{}\n[remember] retain the other action",terminal(obligation.obligation_id,"cancelled")));
    execute_step(3,&[],&mut subject,&tokio_util::sync::CancellationToken::new()).await.unwrap().unwrap();
    let receipts=terminal_receipts(&store);assert_eq!(receipts.len(),2);
    assert!(matches!(receipts[1].outcome,ObligationTerminalOutcome::Rejected { current_state:ObligationTerminalState::Completed,.. }));
    assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"completed");
    assert_eq!(store.committed_intents(Some("memory"),Some("ready"),10).unwrap().len(),1);
    assert_eq!(subject.head.committed_step_no,3);
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.settle_subject_memory().unwrap(),1);
    let export=store.export_cognition(&Default::default()).unwrap();
    assert!(!export.manifest.gaps.iter().any(|gap|gap.blocking),"{:?}",export.manifest.gaps);
}



#[tokio::test]
async fn obligation_projection_keeps_full_bodies_and_charges_the_normal_context_budget() {
    let (_dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    let description="x".repeat(65536);let resume="long resume condition;".repeat(6000);
    let raw=format!("[obligation] {}",serde_json::json!({"description":description,"resume_condition":resume}));
    commit_output(&store,&mut subject,1,&[],&raw).await;
    let prepared=crate::context_projection::prepare_context(&mut subject,&[]).unwrap().unwrap();
    assert_eq!(prepared.obligations[0].description.len(),65536);
    assert_eq!(prepared.obligations[0].resume_condition,resume);
    assert!(prepared.payload.system_prompt.contains(&description));
    assert!(prepared.payload.system_prompt.contains(&resume));
    let projected_tokens=prepared.context.estimated_input_tokens.unwrap();
    let (offline,handle)=shenko_broker::broker::Broker::offline();
    let caps=shenko_broker::request::ProviderCapabilities {
        binding:handle.binding().clone(),max_context_tokens:Some(projected_tokens/2),vision:None,
    };
    subject=subject.with_broker(handle).with_model_capabilities(Some(caps));subject.max_tokens=10;
    assert!(crate::context_projection::prepare_context(&mut subject,&[]).is_err());
    assert_eq!(subject.step.state,SubjectStepState::Planned);
    assert_eq!(store.open_subject_obligations(agent).unwrap()[0].description,description);
    drop(offline);
}

#[tokio::test]
async fn obligation_projection_survives_real_history_compaction() {
    let (_dir,store,agent)=fixture();let (offline,handle)=shenko_broker::broker::Broker::offline();
    let capabilities=shenko_broker::request::ProviderCapabilities {
        binding:handle.binding().clone(),max_context_tokens:Some(8000),vision:None,
    };
    let mut subject=state(store.clone(),agent).unwrap().with_broker(handle)
        .with_model_capabilities(Some(capabilities.clone()));subject.max_tokens=10;
    commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"this exact promise must survive compaction\",\"resume_condition\":\"my later judgment\"}").await;
    let open=store.open_subject_obligations(agent).unwrap();
    for index in 0..24 {
        store.append_subject_raw_fact(&subject.head,EntityId::new(),"user",
            &serde_json::json!({"v":1,"text":format!("older-history-{index}-{}","z".repeat(500))})).unwrap();
    }
    let before=crate::context_projection::prepare_context(&mut subject,&[]).unwrap().unwrap();
    subject.context_registry.install(Arc::new(shenko_compact_budget::BudgetCompact::new(
        shenko_compact_budget::BudgetConfig { trigger_ratio:0.1,recent_ratio:0.01 },
    ))).unwrap();
    let history=shenko_context_policy::StoreHistory::new(store.clone(),EntityId(subject.head.branch_id.0),
        Arc::new(shenko_context_policy::estimate_tokens));
    subject.context_registry.prepare_and_publish(&history,&shenko_context_policy::ModelCaps {
        model_id:capabilities.binding.model().into(),max_context:7990,vision:None,
    },before.context.budget_input_tokens.unwrap()).unwrap();
    let compacted=crate::context_projection::prepare_context(&mut subject,&[]).unwrap().unwrap();
    assert!(matches!(compacted.context.selection,crate::context_projection::ContextSelection::Compacted { .. }));
    assert_eq!(compacted.obligations,open);
    assert!(compacted.payload.system_prompt.contains(&serde_json::to_string(&open).unwrap()));
    assert_eq!(compacted.context.profile_digest,before.context.profile_digest);
    drop(offline);
}

#[tokio::test]
async fn obligation_historical_creation_preserves_terminal_state_and_rejects_identity_drift() {
    let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    let created=commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"historical creation\"}").await;
    let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
    commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,"completed")).await;
    let mut step=store.get_step(&subject.head.branch_id,created).unwrap().unwrap();
    rewrite_decision(&store,&mut step,|decision|decision.schema_version=3);
    let conn=rusqlite::Connection::open(dir.join("store.db")).unwrap();
    conn.execute("UPDATE subject_steps SET data=?2 WHERE step_id=?1",
        rusqlite::params![created.to_string(),serde_json::to_string(&step).unwrap()]).unwrap();
    conn.execute("UPDATE subject_obligations SET decision_ref=?2 WHERE obligation_id=?1",
        rusqlite::params![obligation.obligation_id.to_string(),serde_json::to_string(step.decision_ref.as_ref().unwrap()).unwrap()]).unwrap();
    conn.execute("DELETE FROM subject_intent_steps WHERE step_id=?1",[created.to_string()]).unwrap();
    let before=obligation_row(&dir,obligation.obligation_id);
    store.start_subject_dispatcher().unwrap();
    assert_eq!(obligation_row(&dir,obligation.obligation_id),before);
    let marker:String=conn.query_row("SELECT migration FROM subject_intent_steps WHERE step_id=?1",
        [created.to_string()],|row|row.get(0)).unwrap();assert_eq!(marker,"historical-reconcile");
    conn.execute("DELETE FROM subject_intent_steps WHERE step_id=?1",[created.to_string()]).unwrap();
    conn.execute("UPDATE subject_obligations SET description='different identity'",[]).unwrap();
    assert!(store.start_subject_dispatcher().is_err());
    let count:u64=conn.query_row("SELECT count(*) FROM subject_intent_steps WHERE step_id=?1",[created.to_string()],|row|row.get(0)).unwrap();
    assert_eq!(count,0);
    assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"completed");
}

#[tokio::test]
async fn obligation_modern_recovery_never_infers_a_missing_terminal_settlement() {
    for missing in ["result","intent","marker"] {
        let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
        commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"retain terminal evidence\"}").await;
        let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
        let ended=commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,"cancelled")).await;
        let conn=rusqlite::Connection::open(dir.join("store.db")).unwrap();
        let sql=match missing {
            "result"=>"UPDATE subject_intents SET result_ref=NULL WHERE step_id=?1",
            "intent"=>"DELETE FROM subject_intents WHERE step_id=?1",
            _=>"DELETE FROM subject_intent_steps WHERE step_id=?1",
        };
        conn.execute(sql,[ended.to_string()]).unwrap();
        assert!(store.start_subject_dispatcher().is_err(),"{missing}");
        assert_eq!(obligation_row(&dir,obligation.obligation_id)[10],"cancelled");
        if missing=="marker" {
            let count:u64=conn.query_row("SELECT count(*) FROM subject_intent_steps WHERE step_id=?1",[ended.to_string()],|row|row.get(0)).unwrap();
            assert_eq!(count,0,"schema8 must not be reconstructed as schema3");
        }
    }
}

fn archive(store:&Store,memory:Option<PathBuf>,permit_incomplete:bool)->Vec<u8> {
    let mut export=store.export_cognition(&CognitionSnapshotOptions {
        memory_root:memory,include_git_history:false,..Default::default()
    }).unwrap();
    if permit_incomplete {
        // A malicious sender may hide its self-reported gaps. It still sends
        // actual rows/objects with their computed content fingerprint.
        export.manifest.gaps.clear();
    } else {
        assert!(!export.manifest.gaps.iter().any(|gap|gap.blocking),"{:?}",export.manifest.gaps);
    }
    let mut bytes=Vec::new();export.write_archive(&mut bytes).unwrap();bytes
}

#[tokio::test]
async fn obligation_schema6_archive_reopens_replays_and_relays_exact_settlements() {
    let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"replicated promise\"}").await;
    let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
    commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,"completed")).await;
    let source_row=obligation_row(&dir,obligation.obligation_id);
    let source_receipts=terminal_receipts(&store);let bytes=archive(&store,None,false);
    assert_eq!(store.export_cognition(&Default::default()).unwrap().manifest.schema,6);
    if let Some(output)=std::env::var_os("SHENKO_OBLIGATION_ARCHIVE_DIR") {
        let output=PathBuf::from(output);std::fs::create_dir_all(&output).unwrap();
        std::fs::write(output.join("actual-schema6-terminal.archive"),&bytes).unwrap();
        std::fs::write(output.join("source.json"),serde_json::to_vec_pretty(&serde_json::json!({"source_root":&dir,"agent_id":agent,"obligation_id":obligation.obligation_id})).unwrap()).unwrap();
    }
    let receiver_dir=dir.join("receiver");std::fs::create_dir_all(&receiver_dir).unwrap();
    let receiver=Store::open(receiver_dir.join("store.db"),receiver_dir.join("cas")).unwrap();
    let options=CognitionImportOptions { peer_id:Some(EntityId::new()),..Default::default() };
    for _ in 0..2 {
        let outcome=receiver.import_cognition_archive(&bytes[..],&options).unwrap();
        assert!(outcome.ack(EntityId::new()).verified,"{:?}",outcome.conflicts);
    }
    drop(receiver);
    let receiver=Store::open(receiver_dir.join("store.db"),receiver_dir.join("cas")).unwrap();
    receiver.start_subject_dispatcher().unwrap();
    assert_eq!(terminal_receipts(&receiver),source_receipts);
    assert_eq!(&obligation_row(&receiver_dir,obligation.obligation_id)[..11],&source_row[..11]);
    let relayed=archive(&receiver,None,false);
    let relay_dir=dir.join("relay");std::fs::create_dir_all(&relay_dir).unwrap();
    let relay=Store::open(relay_dir.join("store.db"),relay_dir.join("cas")).unwrap();
    assert!(relay.import_cognition_archive(&relayed[..],&Default::default()).unwrap().ack(EntityId::new()).verified);
    relay.start_subject_dispatcher().unwrap();
    assert_eq!(terminal_receipts(&relay),source_receipts);
    assert!(relay.open_subject_obligations(agent).unwrap().is_empty());
}

#[tokio::test]
async fn obligation_incomplete_archives_publish_no_files_facts_or_baselines() {
    for defect in ["missing_result","divergent_result","rejected_only","missing_intent","missing_marker","missing_projection","policy","decision","seal","open","flip","flip_reverse","reopen_cancelled","invalid_state"] {
        let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
        commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"archive evidence promise\"}").await;
        let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
        let initial=if matches!(defect,"flip_reverse"|"reopen_cancelled") {"cancelled"} else {"completed"};
        let ended=commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,initial)).await;
        let receipt=terminal_receipts(&store).remove(0);
        let conn=rusqlite::Connection::open(dir.join("store.db")).unwrap();
        match defect {
            "missing_result"=>{conn.execute("UPDATE subject_intents SET result_ref=NULL WHERE step_id=?1",[ended.to_string()]).unwrap();},
            "missing_intent"=>{conn.execute("DELETE FROM subject_intents WHERE step_id=?1",[ended.to_string()]).unwrap();},
            "missing_projection"=>{conn.execute("DELETE FROM subject_obligations",[]).unwrap();},
            "missing_marker"=>{conn.execute("DELETE FROM subject_intent_steps WHERE step_id=?1",[ended.to_string()]).unwrap();},
            "policy"=>{let mut payload:serde_json::Value=conn.query_row("SELECT payload FROM subject_intents WHERE step_id=?1",[ended.to_string()],|r|r.get::<_,String>(0)).map(|s|serde_json::from_str(&s).unwrap()).unwrap();
                payload["kind"]["obligation_terminal"]["change"]["reason"]=serde_json::json!("divergent");
                conn.execute("UPDATE subject_intents SET payload=?2 WHERE step_id=?1",rusqlite::params![ended.to_string(),payload.to_string()]).unwrap();},
            "decision"=>{let object=store.get_step(&subject.head.branch_id,receipt.step_id).unwrap().unwrap().input_plan_ref.unwrap();
                conn.execute("UPDATE subject_intents SET decision_ref=?2 WHERE step_id=?1",rusqlite::params![ended.to_string(),serde_json::to_string(&object).unwrap()]).unwrap();},
            "seal"|"divergent_result"|"rejected_only"=>{let mut bad=receipt.clone();
                if defect=="seal" { bad.input_plan_ref=store.get_step(&subject.head.branch_id,
                    obligation_row(&dir,obligation.obligation_id)[2].parse::<EntityId>().unwrap()).unwrap().unwrap().input_plan_ref.unwrap(); }
                else if defect=="rejected_only" { bad.outcome=ObligationTerminalOutcome::Rejected { current_state:ObligationTerminalState::Completed,reason:"request was rejected".into() }; }
                else { bad.change.reason="divergent settlement".into(); }
                conn.execute("UPDATE subject_intents SET result_ref=?2 WHERE step_id=?1",rusqlite::params![ended.to_string(),serde_json::to_string(&bad).unwrap()]).unwrap();},
            "open"|"flip"|"flip_reverse"|"reopen_cancelled"|"invalid_state"=>{conn.execute("UPDATE subject_obligations SET state=?1",[match defect { "open"|"reopen_cancelled"=>"open","flip"=>"cancelled","flip_reverse"=>"completed",_=>"invalid" }]).unwrap();},
            _=>unreachable!(),
        }
        let memory=dir.join("memory");std::fs::create_dir_all(&memory).unwrap();std::fs::write(memory.join("must-not-publish.md"),defect).unwrap();
        let bytes=archive(&store,Some(memory),true);
        let destination=dir.join("target");std::fs::create_dir_all(&destination).unwrap();
        let receiver=Store::open(destination.join("store.db"),destination.join("cas")).unwrap();
        let target_memory=destination.join("memory");
        assert!(receiver.import_cognition_archive(&bytes[..],&CognitionImportOptions {
            memory_root:Some(target_memory.clone()),..Default::default()
        }).is_err(),"{defect}");
        assert!(!target_memory.join("must-not-publish.md").exists(),"{defect}");
        let check=rusqlite::Connection::open(destination.join("store.db")).unwrap();
        for table in ["subject_obligations","subject_steps","subject_intents","subject_intent_steps","cognition_baselines","cognition_file_baselines","cognition_import_order"] {
            let count:u64=check.query_row(&format!("SELECT count(*) FROM {table}"),[],|row|row.get(0)).unwrap();
            assert_eq!(count,0,"{defect}: {table}");
        }
    }
}

#[tokio::test]
async fn obligation_archive_required_conflict_rolls_back_before_file_publication() {
    let (dir,store,agent)=fixture();let mut subject=state(store.clone(),agent).unwrap();
    commit_output(&store,&mut subject,1,&[],"[obligation] {\"description\":\"source promise\"}").await;
    let obligation=store.open_subject_obligations(agent).unwrap().remove(0);
    let memory=dir.join("memory");std::fs::create_dir_all(&memory).unwrap();std::fs::write(memory.join("promise.md"),"before").unwrap();
    let bytes=archive(&store,Some(memory.clone()),false);
    let target=dir.join("target");std::fs::create_dir_all(&target).unwrap();
    let receiver=Store::open(target.join("store.db"),target.join("cas")).unwrap();
    let options=CognitionImportOptions { memory_root:Some(target.join("memory")),peer_id:Some(EntityId::new()),..Default::default() };
    assert!(receiver.import_cognition_archive(&bytes[..],&options).unwrap().conflicts.is_empty());
    let conn=rusqlite::Connection::open(target.join("store.db")).unwrap();
    conn.execute("UPDATE subject_obligations SET description='independent local body'",[]).unwrap();
    let baseline:String=conn.query_row("SELECT group_concat(row_values) FROM cognition_baselines",[],|r|r.get(0)).unwrap();
    let step_count:u64=conn.query_row("SELECT count(*) FROM subject_steps",[],|r|r.get(0)).unwrap();
    commit_output(&store,&mut subject,2,&[],&terminal(obligation.obligation_id,"cancelled")).await;
    std::fs::write(memory.join("promise.md"),"after").unwrap();
    assert!(receiver.import_cognition_archive(&archive(&store,Some(memory),false)[..],&options).is_err());
    assert_eq!(std::fs::read_to_string(target.join("memory/promise.md")).unwrap(),"before");
    assert_eq!(obligation_row(&target,obligation.obligation_id)[10],"open");
    assert!(terminal_receipts(&receiver).is_empty());
    assert_eq!(conn.query_row("SELECT count(*) FROM subject_steps",[],|r|r.get::<_,u64>(0)).unwrap(),step_count);
    assert_eq!(conn.query_row("SELECT group_concat(row_values) FROM cognition_baselines",[],|r|r.get::<_,String>(0)).unwrap(),baseline);
}

/// Admit one input delivery the way a real store receives it. An obligation
/// origin is a durable event, so the fixture admits the event into
/// `inbox_events` instead of only passing it to the step.
fn admit(store: &Store, agent: EntityId, event: &crate::inbox::InboxEvent) -> crate::inbox::InboxEvent {
    let mut batch = shenko_types::store::WriteBatch::new();
    batch.inbox_event_upserts.push(shenko_types::store::RawInboxEvent {
        event_id: event.event_id,
        agent_id: agent,
        source: event.source.clone(),
        kind: "message".into(),
        payload_json: serde_json::to_string(&event.payload).unwrap(),
        received_at_ms: event.created_at.timestamp_millis(),
    });
    store.commit(batch).unwrap();
    event.clone()
}

#[tokio::test]
async fn obligation_origin_outside_the_selected_events_corrects_instead_of_aborting() {
    let (dir, store, agent) = fixture();
    let selected = admit(&store, agent, &message("the input this promise answers"));
    let mut subject = state(store.clone(), agent).unwrap();
    let cancelled = tokio_util::sync::CancellationToken::new();
    cancelled.cancel();
    execute_step(1, &[selected.clone()], &mut subject, &cancelled).await.unwrap();
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    // The model names an event it was never shown. The Store rightly refuses
    // this S0 origin; the loop must return a sourced correction instead of
    // stopping the subject after the result is already durable.
    let unknown = EntityId::new();
    let raw = format!(
        "[obligation] {}\n[remember] this partial decision must not commit",
        serde_json::json!({"description":"answer an input","origin_event_id":unknown}),
    );
    let rejected_id = record_result(&store, &mut subject, &raw);
    let rejected = subject.step.clone();
    let original_head = subject.head.clone();
    assert!(execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
        .await.unwrap().is_none());
    assert_eq!(subject.head.committed_at, original_head.committed_at);
    assert_eq!(subject.head.committed_step_no, 0);
    assert!(store.open_subject_obligations(agent).unwrap().is_empty());
    assert!(store.committed_intents(None, None, 8).unwrap().is_empty());
    let retired = store.get_step(&original_head.branch_id, rejected_id).unwrap().unwrap();
    assert_eq!(retired.state, SubjectStepState::Retired);
    assert_eq!(retired.model_result_ref, rejected.model_result_ref);
    assert_eq!(retired.input_plan_ref, rejected.input_plan_ref);
    assert_eq!(runner::read_sealed(&store, retired.model_result_ref.as_ref().unwrap()).unwrap(), raw.as_bytes());
    let rejection: ControlOutputRejection = serde_json::from_slice(
        &runner::read_sealed(&store, retired.retry_reason_ref.as_ref().unwrap()).unwrap(),
    ).unwrap();
    assert_eq!(rejection.error.code, "CONTROL_PROTOCOL_INVALID");
    assert_eq!(rejection.request_id, rejected.active_request_id.unwrap());
    assert_eq!(rejection.replacement_step_id, subject.step.step_id);
    assert_eq!(rejection.error.details["origin_event_id"], serde_json::json!(unknown));
    assert!(rejection.error.details["reason"].as_str().unwrap().contains("not selected into this step"));
    drop(subject);
    drop(store);

    // The retained result and the replacement survive a closed store.
    let reopened = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(reopened.clone(), agent).unwrap();
    assert_eq!(subject.step.step_id, rejection.replacement_step_id);
    assert_eq!(subject.step.state, SubjectStepState::Preparing);
    let correction = runner::frozen_input(&reopened, &subject.step).unwrap();
    assert_eq!(correction.events.len(), 1);
    assert_eq!(correction.events[0].event_id, selected.event_id);
    // The corrected promise names the event this step really showed.
    let repaired = format!(
        "[obligation] {}",
        serde_json::json!({"description":"answer an input","origin_event_id":selected.event_id}),
    );
    record_result(&reopened, &mut subject, &repaired);
    execute_step(1, &[], &mut subject, &tokio_util::sync::CancellationToken::new())
        .await.unwrap().expect("the corrected S1 commits without another provider request");
    assert_eq!(subject.head.committed_step_no, 1);
    let open = reopened.open_subject_obligations(agent).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].origin_event_id, Some(selected.event_id));
    // The unselected origin never became a committed fact.
    let events: Vec<String> = rusqlite::Connection::open(dir.join("store.db")).unwrap()
        .prepare("SELECT event_id FROM inbox_events").unwrap()
        .query_map([], |row| row.get(0)).unwrap().collect::<rusqlite::Result<_>>().unwrap();
    assert!(!events.iter().any(|event| event == &unknown.to_string()));
}

#[tokio::test]
async fn obligation_creation_keeps_its_sealed_provenance_through_the_publication_migration() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let created = commit_output(&store, &mut subject, 1, &[],
        "[obligation] {\"description\":\"a promise sealed before the publication migration\"}").await;
    let obligation = store.open_subject_obligations(agent).unwrap().remove(0);
    // Recreate the object an older store left behind: same decision, an
    // unversioned publication target and the schema that carried it. Both the
    // step and the creation row named THIS object.
    let mut step = store.get_step(&subject.head.branch_id, created).unwrap().unwrap();
    let mut decision: serde_json::Value = serde_json::from_slice(
        &store.cas.read_object(&step.decision_ref.as_ref().unwrap().digest).unwrap()).unwrap();
    decision["schema_version"] = serde_json::json!(7);
    decision["output"]["schema_version"] = serde_json::json!(4);
    decision["publication_intents"] = serde_json::json!([{
        "intent_id": EntityId::new(),
        "target": {"id": 4242, "channel": "ui", "kind": "artifact"},
        "content": "the legacy artifact body",
        "reply_grant_id": null,
    }]);
    let legacy = store.cas.seal_object(&serde_json::to_vec(&decision).unwrap(),
        "application/json", "decisions", "StepDecision", 7).unwrap().object;
    step.decision_ref = Some(legacy.clone());
    let conn = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    conn.execute("UPDATE subject_steps SET data=?2 WHERE step_id=?1",
        rusqlite::params![created.to_string(), serde_json::to_string(&step).unwrap()]).unwrap();
    conn.execute("UPDATE subject_obligations SET decision_ref=?2 WHERE obligation_id=?1",
        rusqlite::params![obligation.obligation_id.to_string(),
            serde_json::to_string(&legacy).unwrap()]).unwrap();
    drop(conn);
    let before = obligation_row(&dir, obligation.obligation_id);
    assert_eq!(before[8], serde_json::to_string(&legacy).unwrap());
    drop((subject, store));

    // Opening the store runs the explicit publication migration.
    let store = Arc::new(Store::open(dir.join("store.db"), dir.join("cas")).unwrap());
    let mut subject = state(store.clone(), agent).unwrap();
    let migrated = store.get_step(&subject.head.branch_id, created).unwrap().unwrap()
        .decision_ref.unwrap();
    assert_ne!(migrated.digest, legacy.digest, "the step points at the rewritten decision");
    assert_eq!(obligation_row(&dir, obligation.obligation_id), before,
        "the migration rewrites the step, never the immutable creation provenance");
    store.start_subject_dispatcher().unwrap();
    assert_eq!(store.open_subject_obligations(agent).unwrap()[0].description, obligation.description);
    let mut export = store.export_cognition(&CognitionSnapshotOptions::default()).unwrap();
    assert!(!export.manifest.gaps.iter().any(|gap| gap.blocking), "{:?}", export.manifest.gaps);
    let mut bytes = Vec::new();
    export.write_archive(&mut bytes).unwrap();

    // The recorded migration travels with the archive: a peer authenticates
    // the creation reference without inventing provenance for it.
    let receiver_dir = dir.join("receiver");
    std::fs::create_dir_all(&receiver_dir).unwrap();
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let outcome = receiver.import_cognition_archive(&bytes[..], &CognitionImportOptions {
        peer_id: Some(EntityId::new()), ..Default::default()
    }).unwrap();
    assert!(outcome.ack(EntityId::new()).verified, "{:?}", outcome.conflicts);
    assert_eq!(&obligation_row(&receiver_dir, obligation.obligation_id)[..11], &before[..11]);
    drop(receiver);
    // Without the recorded migration the creation reference is unauthenticated.
    let conn = rusqlite::Connection::open(receiver_dir.join("store.db")).unwrap();
    conn.execute("DELETE FROM channel_record_migrations WHERE record_kind='subject_step'", []).unwrap();
    drop(conn);
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let unrecoverable = receiver.export_cognition(&CognitionSnapshotOptions::default()).unwrap();
    assert!(unrecoverable.manifest.gaps.iter().any(|gap|
        gap.blocking && gap.kind == "obligation_required_fact"
        && gap.detail.contains("no recorded publication migration")),
        "{:?}", unrecoverable.manifest.gaps);
    assert!(receiver.start_subject_dispatcher().is_err());

    // A later terminal settlement is a separate step: it changes the
    // projection and the settlement, never the migrated creation.
    commit_output(&store, &mut subject, 2, &[], &terminal(obligation.obligation_id, "completed")).await;
    let after = obligation_row(&dir, obligation.obligation_id);
    assert_eq!(&after[..10], &before[..10], "creation fields stay immutable");
    assert_eq!(after[10], "completed");
}

#[tokio::test]
async fn settled_obligation_settlement_is_never_replaced_by_peer_convergence() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    commit_output(&store, &mut subject, 1, &[],
        "[obligation] {\"description\":\"a promise that ends exactly once\"}").await;
    let obligation = store.open_subject_obligations(agent).unwrap().remove(0);
    commit_output(&store, &mut subject, 2, &[], &terminal(obligation.obligation_id, "completed")).await;
    let receipt = terminal_receipts(&store).remove(0);
    let bytes = archive(&store, None, false);
    let receiver_dir = dir.join("receiver");
    std::fs::create_dir_all(&receiver_dir).unwrap();
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    assert!(receiver.import_cognition_archive(&bytes[..], &options).unwrap()
        .ack(EntityId::new()).verified);
    // The receiver now holds exactly what the peer delivered, and its saved
    // baseline says so. A divergence the peer's delivery cannot explain must
    // still never replace the typed settlement: that row is the receipt of
    // ONE committed S1, not a value to converge.
    let divergent = "{\"divergent\":\"an independently restored copy\"}";
    {
        let conn = rusqlite::Connection::open(receiver_dir.join("store.db")).unwrap();
        conn.execute("UPDATE subject_intents SET result_ref=?2 WHERE intent_id=?1",
            rusqlite::params![receipt.intent_id.to_string(), divergent]).unwrap();
        let stored: String = conn.query_row(
            "SELECT row_values FROM cognition_baselines
             WHERE peer_id=?1 AND table_name='subject_intents' AND entity_key=?2",
            rusqlite::params![peer.to_string(), receipt.intent_id.to_string()], |row| row.get(0)).unwrap();
        let mut values: Vec<(String, Option<String>)> = serde_json::from_str(&stored).unwrap();
        values.iter_mut().filter(|(name, _)| name == "result_ref")
            .for_each(|(_, value)| *value = Some(divergent.to_string()));
        conn.execute("UPDATE cognition_baselines SET row_values=?3
             WHERE peer_id=?1 AND table_name='subject_intents' AND entity_key=?2",
            rusqlite::params![peer.to_string(), receipt.intent_id.to_string(),
                serde_json::to_string(&values).unwrap()]).unwrap();
    }
    drop(receiver);
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let error = match receiver.import_cognition_archive(&bytes[..], &options) {
        Ok(_) => panic!("a settled obligation settlement was replaced by peer convergence"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("cannot be replaced"), "{error:#}");
    let conn = rusqlite::Connection::open(receiver_dir.join("store.db")).unwrap();
    let (state, settlement): (String, String) = conn.query_row(
        "SELECT state,result_ref FROM subject_intents WHERE intent_id=?1",
        [receipt.intent_id.to_string()], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    assert_eq!((state.as_str(), settlement.as_str()), ("settled", divergent));
    assert_eq!(obligation_row(&receiver_dir, obligation.obligation_id)[10], "completed");
    let stored: String = conn.query_row(
        "SELECT row_values FROM cognition_baselines
         WHERE peer_id=?1 AND table_name='subject_intents' AND entity_key=?2",
        rusqlite::params![peer.to_string(), receipt.intent_id.to_string()], |row| row.get(0)).unwrap();
    assert!(stored.contains("an independently restored copy"), "a rejection never advances the baseline");
    let imports: u64 = conn.query_row("SELECT count(*) FROM cognition_import_order", [], |row| row.get(0)).unwrap();
    assert_eq!(imports, 1, "the refused import published nothing");
}

#[tokio::test]
async fn obligation_origin_event_conflict_refuses_the_whole_required_unit() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    commit_output(&store, &mut subject, 1, &[],
        "[obligation] {\"description\":\"an earlier independent promise\"}").await;
    let first = store.open_subject_obligations(agent).unwrap().remove(0);
    let origin = admit(&store, agent, &message("the input two promises answer"));
    // The baseline contains the admitted input before any step disposes it.
    let first_archive = archive(&store, None, false);
    let receiver_dir = dir.join("receiver");
    std::fs::create_dir_all(&receiver_dir).unwrap();
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let peer = EntityId::new();
    let options = CognitionImportOptions { peer_id: Some(peer), ..Default::default() };
    assert!(receiver.import_cognition_archive(&first_archive[..], &options).unwrap()
        .ack(EntityId::new()).verified);
    // Both promises answer the input in the one step that selects it. A later
    // step cannot select an input that an earlier committed step disposed.
    commit_output(&store, &mut subject, 2, &[origin.clone()], &format!(
        "[obligation] {}\n[obligation] {}",
        serde_json::json!({"description":"promise one","origin_event_id":origin.event_id}),
        serde_json::json!({"description":"promise two","origin_event_id":origin.event_id}))).await;
    let second_archive = archive(&store, None, false);
    // The receiving store's own copy of that event changed independently, so
    // the incoming event row is refused. The event and every promise that
    // names it are ONE required unit: publishing either alone would describe
    // a promise about an input the receiver does not hold.
    {
        let conn = rusqlite::Connection::open(receiver_dir.join("store.db")).unwrap();
        conn.execute("UPDATE inbox_events SET data=?2 WHERE event_id=?1",
            rusqlite::params![origin.event_id.to_string(),
                serde_json::json!({"content":"an independently restored body"}).to_string()]).unwrap();
    }
    drop(receiver);
    let receiver = Store::open(receiver_dir.join("store.db"), receiver_dir.join("cas")).unwrap();
    let error = match receiver.import_cognition_archive(&second_archive[..], &options) {
        Ok(_) => panic!("a required obligation unit published around an inbox event conflict"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("required obligation conflict unit diverges"), "{error:#}");
    assert_eq!(receiver.open_subject_obligations(agent).unwrap().len(), 1);
    assert_eq!(receiver.open_subject_obligations(agent).unwrap()[0].obligation_id, first.obligation_id);
    let check = rusqlite::Connection::open(receiver_dir.join("store.db")).unwrap();
    let obligations: u64 = check.query_row("SELECT count(*) FROM subject_obligations", [], |row| row.get(0)).unwrap();
    assert_eq!(obligations, 1, "the refused promise was not published");
    let baselines: u64 = check.query_row(
        "SELECT count(*) FROM cognition_baselines WHERE table_name='subject_obligations'", [], |row| row.get(0)).unwrap();
    assert_eq!(baselines, 1, "no baseline advanced for the refused fact");
    let imports: u64 = check.query_row("SELECT count(*) FROM cognition_import_order", [], |row| row.get(0)).unwrap();
    assert_eq!(imports, 1);
    drop(check);

    // The identical archive publishes both new promises when the event still
    // agrees, alongside the unchanged earlier independent promise.
    let second_dir = dir.join("receiver2");
    std::fs::create_dir_all(&second_dir).unwrap();
    let second = Store::open(second_dir.join("store.db"), second_dir.join("cas")).unwrap();
    assert!(second.import_cognition_archive(&first_archive[..], &options).unwrap().ack(EntityId::new()).verified);
    assert!(second.import_cognition_archive(&second_archive[..], &options).unwrap().ack(EntityId::new()).verified);
    let open = second.open_subject_obligations(agent).unwrap();
    assert_eq!(open.len(), 3);
    assert!(open.iter().any(|obligation| obligation.obligation_id == first.obligation_id));
    assert_eq!(open.iter().filter(|obligation|
        obligation.origin_event_id == Some(origin.event_id)).count(), 2);
}

#[tokio::test]
async fn unreadable_unrelated_decision_keeps_degraded_recovery_and_the_dispatcher() {
    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    let unrelated = commit_output(&store, &mut subject, 1, &[], "[remember] an unrelated decision").await;
    commit_output(&store, &mut subject, 2, &[], "[obligation] {\"description\":\"a promise recovery must still read\"}").await;
    let obligation = store.open_subject_obligations(agent).unwrap().remove(0);
    // This historical step's decision object was never retained, and it holds
    // no obligation fact. Recovery must keep that step degraded instead of
    // aborting the dispatcher or rolling back its retained marker.
    let mut step = store.get_step(&subject.head.branch_id, unrelated).unwrap().unwrap();
    step.decision_ref.as_mut().unwrap().digest =
        shenko_types::object::Digest::blake3(b"a decision object this store never retained");
    let conn = rusqlite::Connection::open(dir.join("store.db")).unwrap();
    conn.execute("UPDATE subject_steps SET data=?2 WHERE step_id=?1",
        rusqlite::params![unrelated.to_string(), serde_json::to_string(&step).unwrap()]).unwrap();
    conn.execute("DELETE FROM subject_intent_steps WHERE step_id=?1", [unrelated.to_string()]).unwrap();
    drop(conn);
    store.start_subject_dispatcher().unwrap();
    let marker: String = rusqlite::Connection::open(dir.join("store.db")).unwrap()
        .query_row("SELECT migration FROM subject_intent_steps WHERE step_id=?1",
            [unrelated.to_string()], |row| row.get(0)).unwrap();
    assert_eq!(marker, "blocked: unreadable historical decision; preserve source and install migrator");
    let open = store.open_subject_obligations(agent).unwrap();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].obligation_id, obligation.obligation_id);
    // Repeated recovery is idempotent and keeps the degraded marker.
    store.start_subject_dispatcher().unwrap();
    assert_eq!(rusqlite::Connection::open(dir.join("store.db")).unwrap()
        .query_row("SELECT migration FROM subject_intent_steps WHERE step_id=?1",
            [unrelated.to_string()], |row| row.get::<_, String>(0)).unwrap(), marker);
}

/// R071/R072 proactivity: a due obligation is admitted as a deterministic
/// SelfWake on the "subject" source, the durable row survives a lost volatile
/// wake through pending_inbox_events, a replayed admission is an exact
/// replay — never a second row — and the staged-input boundary consumes the
/// same lane every other complete input uses (docs/specs/subject-runtime.md#spec-13-8).
#[tokio::test]
async fn due_obligation_admits_deterministic_self_wake_through_input_lanes() {
    use shenko_store::InboxAdmission;

    let (dir, store, agent) = fixture();
    let mut subject = state(store.clone(), agent).unwrap();
    commit_output(&store, &mut subject, 1, &[],
        "[obligation] {\"description\":\"wait out the peer partition\",\"resume_condition\":\"the peer partition heals\",\"due_at\":\"2020-01-01T00:00:00Z\"}").await;
    commit_output(&store, &mut subject, 2, &[],
        "[obligation] {\"description\":\"a promise without a due time\",\"resume_condition\":\"owner judgment\"}").await;
    let obligations = store.open_subject_obligations(agent).unwrap();
    assert_eq!(obligations.len(), 2);
    let due_obligation = obligations.iter().find(|o| o.due_at.is_some()).unwrap();

    let inbox = Arc::new(Inbox::new(8));
    let mut sent = std::collections::HashSet::new();
    crate::dispatch::wake_due_obligations(
        &store, agent, &inbox.sender(), &mut sent).await.unwrap();
    assert_eq!(sent.len(), 1, "only the dated obligation wakes; due_at=None stays unscheduled");
    let delivered = inbox.recv_batch(8, 1_000).await;
    assert_eq!(delivered.len(), 1, "one volatile wake for the durable admission");
    let wake = delivered.into_iter().next().unwrap();
    assert_eq!(wake.kind, crate::inbox::InboxEventKind::SelfWake);
    assert_eq!(wake.source, crate::inbox::SUBJECT_WAKE_SOURCE);
    assert_eq!(wake.payload["obligation_id"],
        serde_json::json!(due_obligation.obligation_id));
    // created_at is the due fact itself, so a later process reproduces the
    // identical original instead of a conflicting same-id row.
    assert_eq!(wake.created_at, due_obligation.due_at.unwrap());
    assert_eq!(crate::dispatch::self_wake_event(due_obligation).event_id, wake.event_id);

    // The volatile channel above was drained by the test, not by a step: the
    // durable row is still pending on the shared recovery lane.
    let pending = store.pending_inbox_events(agent, 64).unwrap();
    assert!(pending.iter().any(|(event_id, _)| *event_id == wake.event_id),
        "the admitted wake survives a lost volatile delivery: {pending:?}");

    // Inside one process the sent-set bounds repeated scans; across a
    // "restart" (fresh set) the deterministic identity admits as an exact
    // replay instead of a second row or a same-id conflict.
    let mut replacement = wake.clone();
    replacement.created_at = chrono::Utc::now();
    assert!(store.admit_inbox_event(wake.event_id, agent,
        crate::inbox::SELF_WAKE_KIND, crate::inbox::SUBJECT_WAKE_SOURCE,
        &serde_json::to_value(&replacement).unwrap()).is_err(),
        "a same-id row with different facts is a conflict, never a silent dup");
    crate::dispatch::wake_due_obligations(
        &store, agent, &inbox.sender(), &mut sent).await.unwrap();
    assert!(inbox.recv_batch(8, 200).await.is_empty(),
        "the same due fact never produces a second wake in one process");
    let mut fresh = std::collections::HashSet::new();
    crate::dispatch::wake_due_obligations(
        &store, agent, &inbox.sender(), &mut fresh).await.unwrap();
    assert_eq!(fresh.len(), 1, "a restarted scanner wakes the still-open fact once");
    assert_eq!(store.admit_inbox_event(wake.event_id, agent,
        crate::inbox::SELF_WAKE_KIND, crate::inbox::SUBJECT_WAKE_SOURCE,
        &serde_json::to_value(&wake).unwrap()).unwrap(), InboxAdmission::Replay);
    drop(inbox.recv_batch(8, 200).await);

    // Staging the wake into a real step consumes it on the same lane as the
    // other complete originals; the sealed S0 cites the wake it selected.
    let commit_seq = subject.step.step_no;
    commit_output(&store, &mut subject, commit_seq, &[wake.clone()], "I pick the due promise back up.").await;
    let consumed: i64 = rusqlite::Connection::open(dir.join("store.db")).unwrap()
        .query_row("SELECT consumed FROM inbox_events WHERE event_id=?1",
            [wake.event_id.to_string()], |row| row.get(0)).unwrap();
    assert_eq!(consumed, 1);
    assert!(store.pending_inbox_events(agent, 64).unwrap().iter()
        .all(|(event_id, _)| *event_id != wake.event_id));
    let committed = store.get_step(&subject.head.branch_id, subject.step.created_by_step_id.unwrap())
        .unwrap().unwrap();
    assert!(runner::frozen_input(&store, &committed).unwrap().events.iter()
        .any(|event| event.event_id == wake.event_id));

    drop((subject, store));
    std::fs::remove_dir_all(dir).unwrap();
}
