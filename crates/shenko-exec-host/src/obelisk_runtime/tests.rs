use super::*;

struct Fixture(PathBuf);
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn runtime() -> Result<Option<(Fixture, ObeliskRuntime)>> {
    let Some(executable) = std::env::var_os("SHENKO_OBELISK_EXECUTABLE") else {
        eprintln!("real Obelisk integration not run: SHENKO_OBELISK_EXECUTABLE is unset");
        return Ok(None);
    };
    let directory =
        std::env::temp_dir().join(format!("shenko-obelisk-proof-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&directory)?;
    let fixture = Fixture(directory.clone());
    let source_checkout =
        PathBuf::from(std::env::var_os("SHENKO_OBELISK_SOURCE_CHECKOUT").context(
            "SHENKO_OBELISK_SOURCE_CHECKOUT must point to the audited independent source checkout",
        )?);
    let arguments = match std::env::var("SHENKO_OBELISK_ARGUMENTS") {
        Ok(value) => serde_json::from_str(&value)?,
        Err(_) => vec![
            source_checkout
                .join("packages/cli/src/obelisk.ts")
                .to_string_lossy()
                .into_owned(),
        ],
    };
    let mut sources = Vec::new();
    for provider in [
        ObeliskProvider::Codex,
        ObeliskProvider::Omp,
        ObeliskProvider::Pi,
    ] {
        let path = directory.join(provider.as_str());
        std::fs::create_dir_all(&path)?;
        sources.push(ObeliskSourceRoot {
            provider,
            root_label: provider.as_str().into(),
            path,
        });
    }
    Ok(Some((
        fixture,
        ObeliskRuntime::new(ObeliskConfig {
            executable: executable.into(),
            source_checkout,
            arguments,
            index_home: directory.join("index"),
            source_instance: "proof-logical-origin".into(),
            sources,
            limits: ObeliskLimits::default(),
        })?,
    )))
}

fn write_jsonl(path: &Path, records: &[Value]) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("fixture file has no parent")?)?;
    let bytes = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    std::fs::write(path, bytes)?;
    Ok(())
}

fn successful(observation: ObeliskObservation) -> Result<Value> {
    ensure!(
        observation.status == ObeliskRunStatus::Succeeded,
        "real CLI query failed: {observation:?}"
    );
    ensure!(
        observation.process.started
            && observation.process.reaped
            && observation.process.readers_drained
            && observation.process.readers_joined,
        "CLI lifetime is not settled"
    );
    observation.data.context("successful response has no data")
}

#[tokio::test]
async fn real_cli_history_preserves_source_locators_and_reads_beyond_projection() -> Result<()> {
    let Some((fixture, runtime)) = runtime()? else {
        return Ok(());
    };
    let long_text = format!("{}🚀原文尾部不能丢", "前".repeat(12_000));
    let pi_path = fixture.0.join("pi/project/session.jsonl");
    let time = "2026-11-15T00:00:00.000Z";
    let pi_records = [
        json!({"type":"session","version":3,"id":"proof-pi","timestamp":time,"cwd":"/proof/project"}),
        json!({"type":"message","id":"m1","parentId":null,"timestamp":time,"message":{"role":"user","content":[{"type":"text","text":"上次权限认证故障最后修改了哪些文件"}],"timestamp":1794700800000_u64}}),
        json!({"type":"message","id":"m2","parentId":"m1","timestamp":"2026-11-15T00:00:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":long_text}],"timestamp":1794700801000_u64}}),
    ];
    write_jsonl(&pi_path, &pi_records)?;
    let mut omp_records = pi_records.clone();
    omp_records[0]["id"] = json!("proof-omp");
    write_jsonl(&fixture.0.join("omp/project/session.jsonl"), &omp_records)?;
    let codex_id = "019e8951-3e7d-7343-a3e3-05bff48a317d";
    write_jsonl(
        &fixture
            .0
            .join(format!("codex/sessions/rollout-{codex_id}.jsonl")),
        &[
            json!({"type":"session_meta","timestamp":time,"payload":{"id":codex_id,"cwd":"/proof/project","timestamp":time,"cli_version":"proof"}}),
            json!({"type":"event_msg","timestamp":time,"payload":{"type":"user_message","message":"source evidence"}}),
        ],
    )?;
    let before = std::fs::read(&pi_path)?;
    let cancel = CancellationToken::new();
    let sessions = successful(
        runtime
            .execute(
                ObeliskQuery::Sessions {
                    provider: None,
                    limit: 20,
                },
                cancel.clone(),
            )
            .await?,
    )?;
    assert_eq!(sessions.as_array().context("sessions shape")?.len(), 3);
    let pi_id = sessions
        .as_array()
        .context("sessions shape")?
        .iter()
        .find(|row| row["source"] == "pi")
        .context("missing Pi session")?["id"]
        .as_str()
        .context("Pi session id is not a string")?
        .to_owned();
    let found = successful(
        runtime
            .execute(
                ObeliskQuery::Search {
                    query: "权限认证".into(),
                    provider: Some(ObeliskProvider::Pi),
                    session_id: None,
                    project_path: None,
                    after: None,
                    before: None,
                    limit: 20,
                    include_inactive: false,
                    literal_fallback: true,
                },
                cancel.clone(),
            )
            .await?,
    )?;
    assert_eq!(found["mode"], "literal_index_fallback");
    assert!(
        !found["hits"]
            .as_array()
            .context("search hits shape")?
            .is_empty()
    );
    assert_eq!(found["full_original_coverage"], false);
    let manifest = runtime
        .execute(
            ObeliskQuery::Manifest {
                after_path: None,
                limit: 20,
            },
            cancel.clone(),
        )
        .await?;
    assert_eq!(manifest.status, ObeliskRunStatus::Succeeded, "{manifest:?}");
    assert_eq!(manifest.source_manifests.len(), 3);
    let source = manifest
        .source_manifests
        .iter()
        .find(|source| source.provider == ObeliskProvider::Pi)
        .context("Pi source manifest is missing")?;
    assert_eq!(source.local_path, pi_path);
    assert_eq!(source.observed_size, Some(before.len() as u64));
    assert_eq!(source.indexed_complete, None);
    assert!(source.source_key.starts_with("session:"));
    let messages = successful(
        runtime
            .execute(
                ObeliskQuery::Messages {
                    session_id: pi_id,
                    cursor: None,
                    limit: 20,
                    include_inactive: false,
                },
                cancel.clone(),
            )
            .await?,
    )?;
    let row = messages["messages"]
        .as_array()
        .context("messages shape")?
        .last()
        .context("Pi message list is empty")?;
    assert!(
        !row["text"]
            .as_str()
            .context("message text is not a string")?
            .contains("原文尾部")
    );
    let message_uuid = row["uuid"]
        .as_str()
        .context("message uuid is not a string")?
        .to_owned();
    let mut offset = 0;
    let mut raw = String::new();
    loop {
        let page = successful(
            runtime
                .execute(
                    ObeliskQuery::RawPage {
                        message_uuid: message_uuid.clone(),
                        offset_utf16: offset,
                        limit_utf16: 4096,
                        include_inactive: false,
                    },
                    cancel.clone(),
                )
                .await?,
        )?;
        assert_eq!(page["status"], "available");
        raw.push_str(
            page["text"]
                .as_str()
                .context("raw page text is not a string")?,
        );
        match page["next_offset_utf16"].as_u64() {
            Some(next) => {
                ensure!(next > offset as u64, "raw pagination did not advance");
                offset = next as usize;
            }
            None => break,
        }
    }
    let recovered: Value = serde_json::from_str(&raw)?;
    assert_eq!(recovered["content"][0]["text"], long_text);
    assert_eq!(
        std::fs::read(&pi_path)?,
        before,
        "queries must not rewrite provider history"
    );
    std::fs::remove_file(&pi_path)?;
    let gone = successful(
        runtime
            .execute(
                ObeliskQuery::RawPage {
                    message_uuid,
                    offset_utf16: 0,
                    limit_utf16: 4096,
                    include_inactive: false,
                },
                cancel,
            )
            .await?,
    )?;
    assert_eq!(gone["status"], "source_unavailable");
    Ok(())
}

#[tokio::test]
async fn real_cli_async_script_is_stopped_by_host_deadline_and_readers_join() -> Result<()> {
    let Some((_fixture, initial)) = runtime()? else {
        return Ok(());
    };
    // Establish that the real CLI, configuration and index work before testing
    // a promise whose asynchronous timer outlives node:vm's synchronous scope.
    successful(
        initial
            .execute(
                ObeliskQuery::Script {
                    script: "return {ready:true};".into(),
                },
                CancellationToken::new(),
            )
            .await?,
    )?;
    let mut config = initial.config().clone();
    config.limits.timeout_ms = 7_000;
    let runtime = ObeliskRuntime::new(config)?;
    let start = tokio::time::Instant::now();
    let stopped = runtime
        .execute(
            ObeliskQuery::Script {
                script:
                    "console.error('shenko_async_timer_entered'); await new Promise(resolve => setTimeout(resolve, 60000)); return {late:true};"
                        .into(),
            },
            CancellationToken::new(),
        )
        .await?;
    assert_eq!(stopped.status, ObeliskRunStatus::TimedOut, "{stopped:?}");
    assert!(
        stopped.process.started && stopped.process.termination_requested && stopped.process.reaped
    );
    assert!(stopped.process.readers_joined && stopped.process.readers_drained);
    assert!(
        stopped
            .process
            .stderr
            .contains("shenko_async_timer_entered"),
        "deadline expired before the asynchronous script was exercised: {stopped:?}"
    );
    assert!(stopped.data.is_none());
    assert!(start.elapsed() < Duration::from_secs(12));
    Ok(())
}
