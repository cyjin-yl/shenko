//! SPEC §6 R117：身份 revision 持久、只追加，重开后可读，改名不换 agent_id。
use shenko_store::store::Store;
use shenko_types::identity::{IdentityChange, IdentitySource, NamingOrigin};
use shenko_types::ids::EntityId;

#[test]
fn identity_revisions_persist_and_reject_duplicate_successor() {
    let dir = std::env::temp_dir().join(format!("shenko-identity-{}", EntityId::new()));
    std::fs::create_dir_all(&dir).unwrap();
    let agent = EntityId::new();
    {
        let store = Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
        assert_eq!(store.ensure_identity(agent, Some("小满")).unwrap().revision, 1);
        // 再次 ensure 不覆盖已有身份。
        assert_eq!(store.ensure_identity(agent, Some("别的")).unwrap().display_name.as_deref(), Some("小满"));
        store
            .change_identity(agent, &IdentityChange::Soul("慢一点，想清楚。".into()), IdentitySource::Operator)
            .unwrap()
            .unwrap();
    }
    let store = Store::open(dir.join("s.db"), dir.join("cas")).unwrap();
    let current = store.latest_identity(agent).unwrap().unwrap();
    assert_eq!((current.revision, current.display_name.as_deref()), (2, Some("小满")));
    assert_eq!(current.naming, NamingOrigin::UserGiven);

    // 两个编辑者基于同一 revision 2 产生的后继：只有一个能落盘。
    let a = current.successor(&IdentityChange::Name("岚".into()), IdentitySource::Operator).unwrap();
    let b = current.successor(&IdentityChange::Name("竞争者".into()), IdentitySource::Operator).unwrap();
    store.append_identity(&a).unwrap();
    assert!(store.append_identity(&b).unwrap_err().to_string().contains("not the successor"));
    let history = store.identity_history(agent).unwrap();
    assert_eq!(history.iter().map(|p| p.revision).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(history.iter().all(|p| p.agent_id == agent));
    assert_eq!(history[2].display_name.as_deref(), Some("岚"));
    drop(store);
    std::fs::remove_dir_all(dir).unwrap();
}
