use shenko_types::ids::{EntityId, LedgerSeq};

use crate::cognition_archive::CognitionRow;
use crate::store::Store;

use super::reconcile::{RecoveryFactOutcome, apply_recovery_fact};
use super::rows::{insert_row, read_baseline, read_existing, set_value, spec_for, write_baseline};

fn projection(store: &Store, state: &str) -> CognitionRow {
    let spec=spec_for("subject_obligations").unwrap();
    let reference=store.cas.seal_object(b"creation", "text/plain", "test", "raw", 1).unwrap().object;
    let mut row=CognitionRow { table:spec.name.into(),key:EntityId::new().to_string(),
        values:spec.columns.iter().map(|name|((*name).into(),None)).collect() };
    let id=row.key.clone();set_value(&mut row,"obligation_id",Some(id));
    for name in ["agent_id","step_id"] { set_value(&mut row,name,Some(EntityId::new().to_string())); }
    set_value(&mut row,"description",Some("immutable creation body".into()));
    set_value(&mut row,"resume_condition",Some("later judgment".into()));
    set_value(&mut row,"decision_ref",Some(serde_json::to_string(&reference).unwrap()));
    set_value(&mut row,"created_seq",Some("1".into()));
    set_value(&mut row,"state",Some(state.into()));row
}

#[test]
fn obligation_merge_has_two_sticky_terminal_branches() {
    // Exercise the real row consumer with and without a peer baseline;
    // these storage-level cases supplement the parser->archive consumers.
    for initial in ["open","completed","cancelled"] {
        for requested in ["open","completed","cancelled"] {
            for has_baseline in [false,true] {
                let root=std::env::temp_dir().join(format!("shenko-obligation-merge-{}",EntityId::new()));
                std::fs::create_dir_all(&root).unwrap();
                let store=Store::open(root.join("store.db"),root.join("cas")).unwrap();
                let mut conn=store.ledger.conn.lock();let tx=conn.transaction().unwrap();
                let spec=spec_for("subject_obligations").unwrap();let peer=EntityId::new();
                let original=projection(&store,initial);
                insert_row(&tx,spec,&original,LedgerSeq(1)).unwrap();
                if has_baseline { write_baseline(&tx,peer,spec.name,&original.key,&original.values).unwrap(); }
                let before=read_baseline(&tx,peer,spec.name,&original.key).unwrap();
                let mut incoming=original.clone();set_value(&mut incoming,"state",Some(requested.into()));
                let outcome=apply_recovery_fact(&tx,spec,&incoming,peer,LedgerSeq(2)).unwrap();
                let allowed=initial==requested || initial=="open";
                assert_eq!(matches!(outcome,RecoveryFactOutcome::Conflict),!allowed,
                    "{initial}->{requested}, baseline={has_baseline}");
                let actual=read_existing(&tx,spec,&incoming).unwrap().unwrap();
                for (name,value) in &actual {
                    assert_eq!(value.as_deref(),if name=="state" && allowed { Some(requested) } else { original.value(name) });
                }
                if !allowed { assert_eq!(read_baseline(&tx,peer,spec.name,&original.key).unwrap(),before); }
            }
        }
    }
}

#[test]
fn obligation_merge_refuses_invalid_inserts_and_creation_replacement() {
    let root=std::env::temp_dir().join(format!("shenko-obligation-insert-{}",EntityId::new()));
    std::fs::create_dir_all(&root).unwrap();let store=Store::open(root.join("store.db"),root.join("cas")).unwrap();
    let mut conn=store.ledger.conn.lock();let tx=conn.transaction().unwrap();
    let spec=spec_for("subject_obligations").unwrap();let peer=EntityId::new();
    let invalid=projection(&store,"unknown");
    assert!(apply_recovery_fact(&tx,spec,&invalid,peer,LedgerSeq(1)).is_err());
    assert!(read_existing(&tx,spec,&invalid).unwrap().is_none());
    let original=projection(&store,"open");
    insert_row(&tx,spec,&original,LedgerSeq(1)).unwrap();
    write_baseline(&tx,peer,spec.name,&original.key,&original.values).unwrap();
    for name in spec.columns.iter().copied().filter(|name|!matches!(*name,"obligation_id"|"state")) {
        let mut incoming=original.clone();set_value(&mut incoming,"state",Some("completed".into()));
        set_value(&mut incoming,name,Some("replacement creation".into()));
        assert!(matches!(apply_recovery_fact(&tx,spec,&incoming,peer,LedgerSeq(2)).unwrap(),RecoveryFactOutcome::Conflict),"{name}");
        assert_eq!(read_existing(&tx,spec,&original).unwrap().unwrap(),original.values);
        assert_eq!(read_baseline(&tx,peer,spec.name,&original.key).unwrap().unwrap(),original.values);
    }
}
