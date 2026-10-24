//! Native child-side driver over an existing trusted task ledger.
//! `fixture-init` creates a fresh synthetic task ONLY for isolated experiments.
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use shenko_exec_ssh::*;
use shenko_store::{Store, TaskInvocation};
use shenko_types::{
    commit::CommitRef, ids::*, object::*, operation::AuthorityRef, store::WriteBatch, task::*,
};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Input {
    db: PathBuf,
    cas: PathBuf,
    context: TaskInvocation,
    operation_id: EntityId,
    logical_key: String,
    target: SshTarget,
    target_id: String,
    receipt_root: String,
    action: SshAction,
    timeout_secs: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mode = args
        .next()
        .context("mode: fixture-init | execute | query | task")?;
    let file = args.next().context("JSON input path")?;
    let input: Input = serde_json::from_slice(&std::fs::read(file)?)?;
    let store = Store::open(input.db, input.cas)?;
    let ctx = input.context;
    let runner = SshTaskRunner { store: &store };
    match mode.as_str() {
        "fixture-init" => {
            // Never overwrite an existing task or give this example a production
            // registry's identity. This mode is explicitly synthetic admission.
            ensure!(
                store.list_recent_tasks(1)?.is_empty(),
                "fixture-init requires an empty ledger"
            );
            let object = store
                .cas
                .seal_object(
                    b"isolated SSH fixture task",
                    "text/plain",
                    "fixture",
                    "fixture",
                    1,
                )?
                .object;
            let zero = CommitRef::new(EntityId::nil(), LedgerSeq::ZERO);
            let task = TaskRecord {
                record_schema_version: 1,
                task_id: ctx.task_id,
                task_revision: ctx.task_revision,
                record_revision: ctx.record_revision,
                agent_id: EntityId::new(),
                conversation_id: EntityId::new(),
                owner: TaskOwner::Conversation {
                    id: EntityId::new(),
                },
                background: false,
                kind: "isolated-ssh-fixture".into(),
                definition_version: 1,
                implementation_ref: ImplementationRef {
                    plugin_id: EntityId::nil(),
                    artifact_digest: Digest::blake3(b"fixture"),
                    registry_revision: 1,
                },
                input_ref: object,
                state: TaskState::Running {
                    checkpoint_ref: None,
                    invocation_id: ctx.invocation_id,
                    mode: RunMode::Run,
                    epoch: ctx.epoch,
                },
                abort_request: None,
                memos: Default::default(),
                residual_case_ids: vec![],
                created_by_step_id: None,
                dispatch_eligibility: DispatchEligibility::Ready,
                no_progress_crashes: 0,
                first_run_at: Some(chrono::Utc::now()),
                terminal_at: None,
                created_commit: zero,
                updated_commit: zero,
            };
            let mut batch = WriteBatch::new();
            batch.task_upserts.push(task);
            store.commit(batch)?;
            println!(
                "{}",
                serde_json::json!({"fixture_initialized":true,"task_id":ctx.task_id})
            );
        }
        "task" => println!(
            "{}",
            serde_json::to_string(&store.external_task(ctx.task_id)?)?
        ),
        "execute" => {
            let binding =
                SshBinding::register(&store, input.target_id, input.target, input.receipt_root)?;
            let authority = AuthorityRef {
                delegation_id: ctx.task_id,
                delegation_revision: RecordRevision(ctx.task_revision.0),
                decision_ref: ctx.invocation_id,
            };
            let observation = runner
                .execute(
                    &ctx,
                    input.operation_id,
                    &input.logical_key,
                    authority,
                    binding,
                    input.action,
                    input.timeout_secs,
                )
                .await?;
            println!("{}", serde_json::to_string(&observation)?);
        }
        "query" => println!(
            "{}",
            serde_json::to_string(&runner.query(input.operation_id, input.timeout_secs).await?)?
        ),
        _ => anyhow::bail!("unsupported mode"),
    }
    Ok(())
}
