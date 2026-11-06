//! Compile this same consumer against separate untouched old5/current6
//! dependency trees. Its APIs and JSON inspection are understood by both.
//! export ROOT ARCHIVE; import ROOT ARCHIVE; relay ROOT ARCHIVE OUTPUT.
//! ROOT/store.db and ROOT/cas may be a retained parser/S1 regression store.
use std::{fs::File,path::PathBuf};
use anyhow::{Context,Result,bail,ensure};
use shenko_store::{Store,CognitionImportOptions,CognitionSnapshotOptions,ARCHIVE_SCHEMA};

fn settlements(store:&Store)->Result<Vec<serde_json::Value>> {
    store.committed_intents(Some("policy"),None,usize::MAX/2)?.into_iter()
        .filter(|intent|intent.contract=="subject/obligation-terminal/1")
        .map(|intent| {
            ensure!(intent.state=="settled","terminal intent is not settled");
            serde_json::from_str(intent.result_ref.as_deref().context("terminal result is missing")?).map_err(Into::into)
        }).collect()
}

fn main()->Result<()> {
    let args:Vec<_>=std::env::args_os().skip(1).collect();
    ensure!((3..=4).contains(&args.len()),"export ROOT ARCHIVE; import ROOT ARCHIVE; relay ROOT ARCHIVE OUTPUT");
    let mode=args[0].to_str().context("mode must be UTF-8")?;
    let root=PathBuf::from(&args[1]);std::fs::create_dir_all(&root)?;
    let db=root.join("store.db");let cas=root.join("cas");let store=Store::open(db.clone(),cas.clone())?;
    if mode=="export" {
        let receipt=store.write_cognition_archive(&CognitionSnapshotOptions::default(),None,File::create(&args[2])?)?;
        ensure!(receipt.complete,"source archive is incomplete: {:?}",receipt.gaps);
        println!("{}",serde_json::json!({"mode":mode,"writer_schema":ARCHIVE_SCHEMA,"receipt":receipt}));
        return Ok(());
    }
    if mode!="import" && mode!="relay" { bail!("unknown consumer mode"); }
    let outcome=store.import_cognition_archive(File::open(&args[2])?,&CognitionImportOptions::default())?;
    ensure!(outcome.conflicts.is_empty() && !outcome.gaps.iter().any(|gap|gap.blocking),"import did not recover the complete archive");
    store.start_subject_dispatcher()?;let expected=settlements(&store)?;drop(store);
    let store=Store::open(db,cas)?;store.start_subject_dispatcher()?;
    let reopened=settlements(&store)?;ensure!(reopened==expected,"reopen changed terminal settlements");
    if mode=="relay" {
        ensure!(args.len()==4,"relay requires an output archive");
        let receipt=store.write_cognition_archive(&CognitionSnapshotOptions::default(),None,File::create(&args[3])?)?;
        ensure!(receipt.complete,"relay archive is incomplete");
    }
    println!("{}",serde_json::json!({"mode":mode,"reader_schema":ARCHIVE_SCHEMA,"fingerprint":outcome.fingerprint,
        "settlements":reopened,"conflicts":outcome.conflicts,"verified_objects":outcome.verified_objects}));
    Ok(())
}
