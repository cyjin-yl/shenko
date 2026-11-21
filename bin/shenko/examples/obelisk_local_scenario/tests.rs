use super::*;
use super::super::fixture_parent;
use base64::Engine as _;
use serde_json::{Value, json};

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Result<Self> {
        let parent = fixture_parent();
        absolute_path(&parent)?;
        create_evidence_dir(&parent)?;
        let root = parent.join(format!("local-scenario-{}", EntityId::new()));
        create_new_directory(&root)?;
        Ok(Self(root))
    }

    fn input(&self) -> Result<(LocalInput, BTreeMap<PathBuf, Vec<u8>>)> {
        let mut roots = Vec::new();
        let mut files = Vec::new();
        let mut originals = BTreeMap::new();
        let long_text = format!("{}🚀精确原文尾部", "前".repeat(12_000));
        let shared = format!("{}\r\n{}\r\n{}\r\n",
            json!({"type":"session","version":3,"id":"shared","timestamp":"2026-11-21T00:00:00Z","cwd":"/fixture/project"}),
            json!({"type":"message","id":"user-1","parentId":null,"message":{"role":"user","content":"original question"}}),
            json!({"type":"message","id":"tool-1","parentId":"user-1","branch_id":"original-branch","message":{"role":"tool","toolCallId":"call-1","content":long_text}}))
            .into_bytes();
        let codex = format!("{}\r\n{}\r\n",
            json!({"type":"session_meta","payload":{"id":"shared","cwd":"/fixture/project"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"preserved native tool result"}}))
            .into_bytes();
        for provider in [ObeliskProvider::Codex, ObeliskProvider::Omp, ObeliskProvider::Pi] {
            let path = self.0.join(format!("original-{}", provider.as_str()));
            let root_label = format!("sample-{}", provider.as_str());
            create_evidence_dir(&path)?;
            // This file must never be enumerated or copied by explicit capture.
            write_bytes(&path.join("unselected.jsonl"), b"unselected private material\n")?;
            let relative_path = if provider == ObeliskProvider::Codex {
                "sessions/2026/11/rollout-shared.jsonl"
            } else {
                "project/session.jsonl"
            };
            let bytes = if provider == ObeliskProvider::Codex { &codex } else { &shared };
            let file = SelectedFile {
                source: ExternalHistorySource {
                    provider: provider.as_str().into(),
                    source_instance: "selected-history-proof".into(),
                    source_key: format!("session:{}:shared", provider.as_str()),
                    session_id: Some(format!("{}:shared", provider.as_str())),
                },
                identity_basis: IdentityBasis::NativeSession,
                relative_path: relative_path.into(), bytes: bytes.len() as u64,
                digest: Digest::blake3(bytes),
            };
            let original = path.join(relative_path);
            create_evidence_dir(original.parent().context("fixture parent")?)?;
            write_bytes(&original, bytes)?;
            originals.insert(original, bytes.clone());
            files.push(file);
            if provider == ObeliskProvider::Codex {
                let relative_path = "sessions/2026/11/subagents/child.jsonl";
                let bytes = b"{\"id\":\"child-tool\",\"content\":\"raw child output\"}\n{\"incomplete\":\"opaque tail";
                let original = path.join(relative_path);
                create_evidence_dir(original.parent().context("child fixture parent")?)?;
                write_bytes(&original, bytes)?;
                originals.insert(original, bytes.to_vec());
                files.push(SelectedFile {
                    source: ExternalHistorySource {
                        provider: "codex".into(), source_instance: "selected-history-proof".into(),
                        source_key: format!("root:{root_label}/{relative_path}"), session_id: None,
                    },
                    identity_basis: IdentityBasis::ConfiguredRootRelativePath,
                    relative_path: relative_path.into(), bytes: bytes.len() as u64,
                    digest: Digest::blake3(bytes),
                });
            }
            roots.push(ObeliskSourceRoot { provider, root_label, path });
        }
        Ok((LocalInput {
            schema_version: 1, scenario: "explicit-originals".into(),
            question: "这段历史记录说了什么？请标明来源和未知项。".into(),
            rubric: "根据实际读取的完整来源回答，保留原文归因；没有证据不得断言。".into(),
            source_instance: "selected-history-proof".into(),
            limits: CaptureLimits { max_files: 8, max_file_bytes: 1024 * 1024,
                max_total_bytes: 4 * 1024 * 1024 }, roots, files,
        }, originals))
    }

    fn selection(&self, name: &str, input: &LocalInput) -> Result<PathBuf> {
        let path = self.0.join(format!("{name}.json"));
        write_bytes(&path, &serde_json::to_vec_pretty(input)?)?;
        Ok(path)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn complete_native_layouts_archive_and_reopen_without_original_roots() -> Result<()> {
    let fixture = Fixture::new()?;
    let (input, originals) = fixture.input()?;
    let input_path = fixture.selection("owner-selection", &input)?;
    let captured = FrozenScenario::capture(&input_path, &fixture.0.join("captured"))?;
    let digest = captured.manifest_digest().clone();
    let manifest = captured.manifest();
    assert_eq!(manifest.file_count, 4);
    assert_eq!(manifest.total_bytes, originals.values().map(|bytes| bytes.len() as u64).sum::<u64>());
    assert_eq!(manifest.question_digest, Digest::blake3(input.question.as_bytes()));
    assert_eq!(manifest.rubric_digest, Digest::blake3(input.rubric.as_bytes()));
    for root in &input.roots { std::fs::remove_dir_all(&root.path)?; }
    std::fs::remove_file(input_path)?;
    let reopened = FrozenScenario::open(&fixture.0.join("captured"), &digest)?;
    assert_eq!(reopened.manifest(), captured.manifest());
    let mounted = reopened.mount(&fixture.0.join("mounted"))?;
    assert_eq!(mounted.manifest_digest(), &digest);
    let upstream = fixture.0.join("audited-upstream");
    let template = ObeliskConfig {
        executable: fixture.0.join("node"),
        arguments: vec![upstream.join("packages/cli/src/obelisk.ts").to_string_lossy().into_owned()],
        source_checkout: upstream, index_home: fixture.0.join("unused-template-index"),
        source_instance: "unused-template-instance".into(),
        sources: input.roots.clone(), limits: Default::default(),
    };
    let configuration = mounted.configuration(&template, &fixture.0.join("fresh-index"))?;
    configuration.validate()?;
    assert_eq!(configuration.sources.len(), 3);
    assert_eq!(configuration.source_instance, input.source_instance);
    for root in &configuration.sources {
        assert_eq!(root.path, fixture.0.join("mounted/originals").join(root.provider.as_str()));
        assert!(!root.path.join("unselected.jsonl").exists());
    }
    let store = Store::open(fixture.0.join("archive.db"), fixture.0.join("cas"))?;
    let agent_id = EntityId::new();
    let archives = mounted.archive_into(&store, agent_id)?;
    assert_eq!(archives.len(), 4);
    for (selected, archive) in mounted.manifest().files.iter().zip(&archives) {
        let provider = input.roots.iter().find(|root|
            root.provider.as_str() == selected.source.provider).context("fixture provider")?;
        let original = &originals[&provider.path.join(&selected.relative_path)];
        let page = external_history::read_original(&store, agent_id, archive.archive_session_id, 0, 65_536, false)?;
        assert!(page.authenticated && page.next_offset.is_none());
        assert_eq!(base64::engine::general_purpose::STANDARD.decode(page.content_base64)?, *original);
        assert_eq!(archive.raw_ref.digest, selected.digest);
        assert_eq!(archive.source, selected.source);
    }
    let omp = archives.iter().find(|archive| archive.source.provider == "omp").context("OMP archive")?;
    let pi = archives.iter().find(|archive| archive.source.provider == "pi").context("Pi archive")?;
    assert_eq!(omp.raw_ref.digest, pi.raw_ref.digest);
    assert_ne!(omp.archive_session_id, pi.archive_session_id);
    let hits = external_history::search(&store, agent_id, "精确原文尾部", None, None, 10)?;
    assert_eq!(hits.hits.len(), 2);
    assert_ne!(hits.hits[0].event_id, hits.hits[1].event_id);
    assert!(archives.iter().any(|archive| archive.invalid_records > 0));
    mounted.validate()?;
    Ok(())
}

#[test]
fn invalid_selections_fail_before_any_original_or_snapshot_is_opened() -> Result<()> {
    let fixture = Fixture::new()?;
    let (valid, _) = fixture.input()?;
    for root in &valid.roots { std::fs::remove_dir_all(&root.path)?; }
    let mut variants = Vec::new();
    let mut wrong_identity = valid.clone();
    wrong_identity.files[0].source.source_key = "session:another-session".into();
    variants.push(wrong_identity);
    let mut unqualified = valid.clone();
    unqualified.files[0].source.session_id = Some("shared".into());
    unqualified.files[0].source.source_key = "session:shared".into();
    variants.push(unqualified);
    let mut duplicate = valid.clone();
    duplicate.files.push(duplicate.files[0].clone());
    variants.push(duplicate);
    let mut traversal = valid.clone();
    traversal.files[0].relative_path = "../unselected.jsonl".into();
    variants.push(traversal);
    let mut over_budget = valid.clone();
    over_budget.limits.max_total_bytes = 1;
    variants.push(over_budget);
    let mut foreign_instance = valid.clone();
    foreign_instance.files[0].source.source_instance = "foreign-instance".into();
    variants.push(foreign_instance);
    for (index, input) in variants.iter().enumerate() {
        let selection = fixture.selection(&format!("rejected-{index}"), input)?;
        let destination = fixture.0.join(format!("never-created-{index}"));
        let error = FrozenScenario::capture(&selection, &destination)
            .err().context("invalid selection was accepted")?;
        assert!(!format!("{error:#}").contains("selected provider root unavailable"));
        assert!(!destination.exists());
    }
    let too_large = fixture.0.join("oversized-input.json");
    write_bytes(&too_large, &vec![b' '; usize::try_from(MAX_INPUT_BYTES + 1)?])?;
    assert!(FrozenScenario::capture(&too_large, &fixture.0.join("oversized-output")).is_err());
    assert!(!fixture.0.join("oversized-output").exists());
    let selection = fixture.selection("valid-but-unavailable", &valid)?;
    assert!(FrozenScenario::capture(&selection, &fixture.0.join("unavailable-output")).is_err());
    assert!(!fixture.0.join("unavailable-output").exists());
    Ok(())
}

#[test]
fn changed_bytes_foreign_bindings_and_extra_mounted_sources_are_rejected() -> Result<()> {
    let fixture = Fixture::new()?;
    let (input, originals) = fixture.input()?;
    let selection = fixture.selection("selected-version", &input)?;
    let first = &input.files[0];
    let root = input.roots.iter().find(|root|
        root.provider.as_str() == first.source.provider).context("first provider")?;
    let original_path = root.path.join(&first.relative_path);
    let original = &originals[&original_path];
    let mut grown = original.clone();
    grown.extend_from_slice(b"unselected later append\n");
    std::fs::write(&original_path, &grown)?;
    assert!(FrozenScenario::capture(&selection, &fixture.0.join("grown-input")).is_err());
    assert!(!fixture.0.join("grown-input/snapshot.json").exists());
    let mut changed = original.clone();
    changed[0] = b'!';
    std::fs::write(&original_path, &changed)?;
    assert!(FrozenScenario::capture(&selection, &fixture.0.join("changed-input")).is_err());
    assert!(!fixture.0.join("changed-input/snapshot.json").exists());
    std::fs::write(&original_path, original)?;
    let captured = FrozenScenario::capture(&selection, &fixture.0.join("captured"))?;
    assert!(FrozenScenario::open(&fixture.0.join("captured"), &Digest::blake3(b"foreign run")).is_err());
    let mounted = captured.mount(&fixture.0.join("mounted"))?;
    let selected = &mounted.manifest().files[0];
    let selected_path = super::original_path(&fixture.0.join("mounted"), selected);
    let mut bytes = std::fs::read(&selected_path)?;
    bytes[0] = b'!';
    std::fs::write(&selected_path, &bytes)?;
    assert!(mounted.validate().is_err());
    captured.validate()?; // Independent copies: changing the arm cannot alter the captured original.
    let extra = captured.mount(&fixture.0.join("extra-source"))?;
    write_bytes(&fixture.0.join("extra-source/originals/codex/unselected.jsonl"), b"extra\n")?;
    assert!(extra.validate().is_err());
    let mut rebound = captured.manifest().clone();
    rebound.question.push_str(" changed question");
    std::fs::write(fixture.0.join("captured/snapshot.json"), serde_json::to_vec_pretty(&rebound)?)?;
    assert!(FrozenScenario::open(&fixture.0.join("captured"), captured.manifest_digest()).is_err());
    Ok(())
}

// Retained JSON below is a deterministic test fixture. It does not manufacture
// native T1/E1/T2 execution, a model wire, a judge score or runtime acceptance.
#[test]
fn local_runner_bindings_and_original_reader_results_reconstruct_offline() -> Result<()> {
    use super::super as runner;
    use shenko_exec_host::PreparedSandbox;
    use shenko_types::task::{NativeTaskDefinition, TaskOutcome};
    use std::ffi::OsString;

    assert!(matches!(runner::parse_command(Vec::<OsString>::new())?, runner::Command::Run(None)));
    assert!(matches!(runner::parse_command(["--local-scenario", "/owner/input.json"].map(OsString::from))?,
        runner::Command::Run(Some(_))));
    assert!(matches!(runner::parse_command(["--report-only", "/retained/run"].map(OsString::from))?,
        runner::Command::ReportOnly(_)));
    for arguments in [vec!["--local-scenario"], vec!["--report-only"], vec!["--unknown", "/input"],
        vec!["--local-scenario", "/input", "--report-only", "/run"],
        vec!["--report-only", "--local-scenario"]] {
        assert!(runner::parse_command(arguments.into_iter().map(OsString::from)).is_err());
    }
    let fixture = Fixture::new()?;
    let (input, _) = fixture.input()?;
    let input_path = fixture.selection("selected-for-runner", &input)?;
    let run_root = fixture.0.join("retained-run");
    create_evidence_dir(&run_root)?;
    let scenario = runner::SelectedScenario::Local(Box::new(FrozenScenario::capture(&input_path,
        &run_root.join("local-scenario"))?));
    assert_eq!(scenario.scenario(), input.scenario);
    assert_eq!(scenario.question(), input.question);
    assert_eq!(scenario.rubric(), input.rubric);
    let snapshot = scenario.local().context("local scenario")?;
    let selection_binding = scenario.binding()?;
    let budget = json!({"arm_deadline_ms":180000,"context_tokens":16384,
        "judge_max_output_tokens":8192,"sampling":"unchanged production defaults"});
    let run_binding = json!({"schema_version":2,"run_id":EntityId::new(),"scenario_kind":"local",
        "scenario":scenario.scenario(),"corpus_digest":scenario.corpus_digest(),
        "source_selection":selection_binding,"message_digest":Digest::blake3(scenario.question().as_bytes()),
        "rubric_digest":Digest::blake3(scenario.rubric().as_bytes()),"budget":budget});
    runner::write_json(&run_root.join("run-binding.json"), &run_binding)?;
    assert!(!run_root.join("fixture.json").exists());
    // Source locators in selection.json are now unavailable. The runner helpers
    // and offline reader must use only the original retained snapshot.
    for root in &input.roots { std::fs::remove_dir_all(&root.path)?; }
    std::fs::remove_file(input_path)?;
    let mut original_candidate = None;
    for candidate in [false, true] {
        let name = runner::condition_name(candidate);
        let arm_root = run_root.join(name);
        create_evidence_dir(&arm_root)?;
        create_evidence_dir(&arm_root.join("native-results"))?;
        let prepared = PreparedSandbox::fresh(name, &arm_root)?;
        let installation = prepared.installation();
        let mounted_root = installation.join("local-scenario");
        let mounted = snapshot.mount(&mounted_root)?;
        let facts = runner::local_source_facts(&mounted, &mounted_root, &prepared, candidate)?;
        assert!(runner::local_facts_bound(&facts, snapshot)?);
        assert!(facts.iter().filter(|fact| fact["kind"] == "record").all(|fact|
            candidate == fact["event_id"].is_null()));
        if !candidate {
            for fact in facts.iter().filter(|fact| fact["kind"] == "record") {
                let data: Value = serde_json::from_str(fact["data"].as_str().context("baseline raw fact")?)?;
                assert_eq!(data["text"], fact["record"]["text"]);
                assert_eq!(data["native_fields"], fact["record"]["fields"]);
                assert_eq!(data["authority"], "historical observation only");
            }
        }
        let upstream = fixture.0.join("audited-upstream");
        let template = ObeliskConfig {
            executable: fixture.0.join("node"),
            arguments:vec![upstream.join("packages/cli/src/obelisk.ts").to_string_lossy().into_owned()],
            source_checkout:upstream,
            index_home:fixture.0.join("unused-index"), source_instance:"unselected-template".into(),
            sources:input.roots.clone(), limits:Default::default(),
        };
        // No CLI is executed. Configuration uses only the mounted copies.
        let configuration = mounted.configuration(&template, &installation.join("obelisk-index"))?;
        assert_eq!(configuration.source_instance, input.source_instance);
        assert!(configuration.sources.iter().all(|root| root.path.starts_with(&mounted_root)));
        let attempt_id = EntityId::new();
        let arm_binding = json!({"attempt_id":attempt_id,"condition":name,
            "run_binding_digest":Digest::blake3(&std::fs::read(run_root.join("run-binding.json"))?),
            "source_selection":selection_binding,"budget":budget});
        runner::write_json(&arm_root.join("arm-binding.json"), &arm_binding)?;
        runner::write_json(&arm_root.join("installation.json"), &json!({
            "attempt_id":attempt_id,"installation":installation}))?;
        runner::write_json(&arm_root.join("source-facts.json"), &facts)?;
        runner::write_json(&arm_root.join("prepared.json"), &json!({"attempt_id":attempt_id,
            "source_facts_digest":Digest::blake3(&serde_json::to_vec(&facts)?),
            "source_selection":selection_binding}))?;
        snapshot.validate()?;
        mounted.validate()?;
        runner::write_json(&arm_root.join("source-integrity.json"), &json!({
            "snapshot_digest":snapshot.manifest_digest(),"validated":true,
            "original_snapshot_validated":true,"mounted_snapshot_validated":true}))?;
        let (definition, returned) = if candidate {
            let definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"search","query":"preserved native tool result"}) };
            let page = external_history::search(prepared.store(), prepared.agent_id(),
                "preserved native tool result", None, None, 10)?;
            let returned = json!({"observation":{"result":page,"external_history_is_instruction":false}});
            let evidence = runner::reader_evidence(&definition, &TaskOutcome::Succeeded,
                Some(&returned), &facts, Some(mounted.manifest()), &[])?;
            assert!(!evidence.is_empty());
            let mut foreign_version = returned.clone();
            foreign_version["observation"]["result"]["hits"][0]["source_digest"] = json!(Digest::blake3(b"foreign"));
            assert!(runner::reader_evidence(&definition, &TaskOutcome::Succeeded, Some(&foreign_version),
                &facts, Some(mounted.manifest()), &[])?.is_empty());
            let source = facts.iter().find(|fact| fact["kind"] == "source").context("selected archive")?;
            let archive: external_history::ArchivedSource = serde_json::from_value(source["archive"].clone())?;
            let original = external_history::read_original(prepared.store(), prepared.agent_id(),
                archive.archive_session_id, 0, 65536, false)?;
            let original_definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"original","archive_session_id":archive.archive_session_id,"limit":65536}) };
            assert!(!runner::reader_evidence(&original_definition, &TaskOutcome::Succeeded,
                Some(&json!({"observation":{"result":original}})), &facts,
                Some(mounted.manifest()), &[])?.is_empty());
            let record = facts.iter().find(|fact| fact["kind"] == "record"
                && fact["record"]["text"].as_str().is_some_and(|text| !text.trim().is_empty()))
                .context("selected native record")?;
            let mut page = record["record"].clone();
            let full_text = page["text"].as_str().context("complete projection")?;
            let total = full_text.chars().count();
            let text: String = full_text.chars().take(4096).collect();
            let count = text.chars().count();
            page["text"] = json!(text);
            page["page"] = json!({"offset_chars":0,"returned_chars":count,"total_chars":total,
                "next_offset_chars":(count < total).then_some(count)});
            let read_definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"read","event_id":page["event_id"]}) };
            assert!(!runner::reader_evidence(&read_definition, &TaskOutcome::Succeeded,
                Some(&json!({"observation":{"result":page}})), &facts,
                Some(mounted.manifest()), &[])?.is_empty());
            let import = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"archive"}) };
            assert!(runner::reader_evidence(&import, &TaskOutcome::Succeeded,
                Some(&json!({"observation":{"result":archive}})), &facts,
                Some(mounted.manifest()), &[])?.is_empty());
            let selected = mounted.manifest().files.iter().find(|file|
                file.source.session_id.is_some()).context("native selected session")?;
            let session = selected.source.session_id.as_deref().context("native session id")?;
            let cli_definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"obelisk","request":{"op":"search","query":"arbitrary"}}) };
            let cli = runner::tests::cli_result(&input.source_instance,
                json!({"mode":"literal_index_fallback","hits":[{
                    "uuid":"native-message","session_id":session,"text":"observed source text"}]}));
            let cli_evidence = runner::reader_evidence(&cli_definition, &TaskOutcome::Succeeded,
                Some(&cli), &facts, Some(mounted.manifest()), &[])?;
            assert_eq!(cli_evidence.len(), 1);
            assert_eq!(cli_evidence[0]["source_digest"], json!(selected.digest));
            let mut foreign = cli.clone();
            foreign["observation"]["result"]["source_instance"] = json!("foreign-instance");
            assert!(runner::reader_evidence(&cli_definition, &TaskOutcome::Succeeded, Some(&foreign),
                &facts, Some(mounted.manifest()), &[])?.is_empty());
            foreign = cli.clone();
            foreign["observation"]["result"]["data"]["hits"][0]["session_id"] = json!("codex:unselected");
            assert!(runner::reader_evidence(&cli_definition, &TaskOutcome::Succeeded, Some(&foreign),
                &facts, Some(mounted.manifest()), &[])?.is_empty());
            let raw_definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"obelisk","request":{"op":"raw_page","message_uuid":"native-message"}}) };
            let raw = runner::tests::cli_result(&input.source_instance, json!({"status":"available",
                "message_uuid":"native-message","text":"observed source text","offset":0,
                "offset_unit":"utf16_code_units"}));
            assert!(runner::reader_evidence(&raw_definition, &TaskOutcome::Succeeded, Some(&raw),
                &facts, Some(mounted.manifest()), &[])?.is_empty());
            let prior = json!({"authenticated":true,"definition":cli_definition,
                "result":runner::tests::retained_result(cli)?});
            assert_eq!(runner::reader_evidence(&raw_definition, &TaskOutcome::Succeeded, Some(&raw),
                &facts, Some(mounted.manifest()), &[&prior])?.len(), 1);
            let manifest_definition = NativeTaskDefinition { kind:runner::obelisk::TASK_KIND.into(), definition_version:1,
                input:json!({"op":"obelisk","request":{"op":"manifest"}}) };
            let mut native_manifest = runner::tests::cli_result(&input.source_instance, json!({}));
            native_manifest["observation"]["result"]["source_manifests"] = json!([{
                "source_instance":input.source_instance,"provider":selected.source.provider,
                "source_key":selected.source.source_key,"identity_basis":"native_session",
                "session_id":session,"local_path":facts.iter().find(|fact| fact["kind"] == "source"
                    && fact["archive"]["source"] == json!(selected.source)).context("selected locator")?["local_path"],
                "availability":"available","observed_size":selected.bytes,"records":[{"id":"native-message"}],
                "records_truncated":false
            }]);
            let manifest_prior = json!({"authenticated":true,"definition":manifest_definition,
                "result":runner::tests::retained_result(native_manifest.clone())?});
            assert!(runner::reader_evidence(&manifest_definition, &TaskOutcome::Succeeded, Some(&native_manifest),
                &facts, Some(mounted.manifest()), &[])?.is_empty(), "metadata alone is not a source read");
            assert_eq!(runner::reader_evidence(&raw_definition, &TaskOutcome::Succeeded, Some(&raw),
                &facts, Some(mounted.manifest()), &[&manifest_prior])?.len(), 1);
            (definition, returned)
        } else {
            let fact = facts.iter().find(|fact| fact["kind"] == "record").context("baseline projection")?;
            let definition = NativeTaskDefinition { kind:"history.query".into(), definition_version:1,
                input:json!({"op":"search"}) };
            let returned = json!({"schema_version":1,"status":"ok","op":"search","events":[{
                "event_id":fact["event_id"],"session_id":fact["session_id"],"seq":fact["seq"],
                "event_ref":"retained-source-event","data_preview":fact["data"]}]});
            (definition, returned)
        };
        let result = runner::tests::retained_result(returned)?;
        let record = json!({"schema_version":1,"authenticated":true,"task_kind":definition.kind,
            "definition":definition,"result":result,"retrieved_evidence":runner::reader_evidence(
                &definition, &result.outcome, result.result.as_ref(), &facts, Some(mounted.manifest()), &[])?});
        assert!(!record["retrieved_evidence"].as_array().context("reader evidence")?.is_empty());
        runner::write_json(&arm_root.join("native-results/original.json"), &record)?;
        if candidate {
            let mut foreign = record.clone();
            foreign["result"] = json!(runner::tests::retained_result(result.result.clone().context("result body")?)?);
            runner::write_json(&arm_root.join("native-results/foreign.json"), &foreign)?;
            original_candidate = Some(record);
        }
        runner::write_json(&arm_root.join("committed.json"), &json!({"installation":installation,
            "consumed_results":[result],"output":"offline fixture continuation"}))?;
        runner::write_json(&arm_root.join("failure.json"), &json!({"condition":name,"attempt_id":attempt_id,
            "status":"failed","effective":null,"used_expected_reader":false,
            "error":"original interrupted fixture; no subject inference executed"}))?;
    }
    runner::write_json(&run_root.join("cleanup.json"), &json!({"cleanup_ok":true,"fixture":true}))?;
    let first = runner::reconstruct_report(&run_root)?;
    assert_eq!(first, runner::reconstruct_report(&run_root)?);
    assert_eq!(first["scenario"], input.scenario);
    assert_eq!(first["source_selection"], selection_binding);
    assert_eq!(first["corpus_digest"], json!(scenario.corpus_digest()));
    assert_eq!(first["complete"], false);
    assert_eq!(first["reconstruction"]["requests_issued"], false);
    assert!(first["arms"].as_array().context("arms")?.iter().all(|arm|
        arm["status"] == "failed" && arm["used_expected_reader"] == true));
    let candidate_root = run_root.join("candidate");
    let original_bytes = std::fs::read(candidate_root.join("native-results/original.json"))?;
    std::fs::remove_file(candidate_root.join("native-results/original.json"))?;
    let foreign_only = runner::reconstruct_report(&run_root)?;
    assert_eq!(foreign_only["arms"][1]["used_expected_reader"], false);
    write_bytes(&candidate_root.join("native-results/original.json"), &original_bytes)?;
    let mut unbound = first["arms"][1].clone();
    unbound["source_integrity"]["validated"] = json!(false);
    assert!(!runner::retained_reader_admitted(&unbound,
        unbound["retained_native_results"].as_array().context("retained results")?,
        runner::obelisk::TASK_KIND, &scenario)?);
    let mut wrong_run = run_binding.clone();
    wrong_run["source_selection"]["snapshot_digest"] = json!(Digest::blake3(b"foreign snapshot"));
    assert!(runner::open_retained_scenario(&run_root, &wrong_run).is_err());
    assert_eq!(serde_json::from_slice::<Value>(&original_bytes)?, original_candidate.context("original candidate")?);
    assert!(!run_root.join("report.json").exists());
    write_bytes(&run_root.join("local-scenario/originals/codex/unselected.jsonl"), b"foreign source\n")?;
    assert!(runner::reconstruct_report(&run_root).is_err());
    Ok(())
}
