use anyhow::{Context, Result, ensure};
use shenko_seed::{Bootstrap, Capability, CapabilityStatus, HomeManifest, SeedConfig};
use shenko_types::ids::EntityId;

#[test]
fn legacy_cow_declaration_upgrades_only_after_guest_home_readback() -> Result<()> {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/seed-home-fixtures")
        .join(EntityId::new().to_string());
    let bootstrap = Bootstrap::new(SeedConfig { data_dir: root.clone(), ..Default::default() });
    let installation_id = EntityId::new();
    let prepared = bootstrap.prepare(installation_id)?;
    let manifest_path = prepared.manifest_path.clone();
    let mut legacy = prepared.manifest.clone();
    let home_file = legacy.home_dir().join("personal-state");
    let memory_file = legacy.memory_dir().join("remembered-fact");
    std::fs::write(&home_file, b"persistent guest home")?;
    std::fs::write(&memory_file, b"persistent memory")?;
    legacy.capabilities.insert("copy_on_write_mounts".into(), Capability {
        status: CapabilityStatus::Unsupported,
        detail: "COW requests fail explicitly; never downgraded to direct RW".into(),
    });
    drop(prepared);
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&legacy)?)?;

    let reopened = Bootstrap::new(SeedConfig { data_dir: root.clone(), ..Default::default() })
        .prepare(installation_id)?;
    ensure!(reopened.manifest.capabilities.get("copy_on_write_mounts")
        .context("COW capability")?.status == CapabilityStatus::Available,
        "the exact legacy declaration was not upgraded");
    ensure!(reopened.manifest.installation_id == installation_id
        && reopened.manifest.mounts == legacy.mounts
        && reopened.manifest.created_at == legacy.created_at,
        "capability migration changed the installation, mounts, or original creation time");
    ensure!(std::fs::read(&home_file)? == b"persistent guest home"
        && std::fs::read(&memory_file)? == b"persistent memory",
        "capability migration replaced persisted home or memory bytes");
    let persisted: HomeManifest = serde_json::from_slice(&std::fs::read(&manifest_path)?)?;
    ensure!(persisted == reopened.manifest, "upgraded declaration was not persisted");
    drop(reopened);

    // A different declaration is not this one legacy migration. Refusal must
    // preserve both the manifest bytes and the installation's existing data.
    let mut foreign = persisted;
    foreign.capabilities.get_mut("copy_on_write_mounts").context("COW capability")?
        .detail = "unrecognized capability declaration".into();
    let foreign_bytes = serde_json::to_vec_pretty(&foreign)?;
    std::fs::write(&manifest_path, &foreign_bytes)?;
    let error = bootstrap.prepare(installation_id).err().context("foreign declaration was accepted")?;
    ensure!(format!("{error:#}").contains("SEED_CLOSURE_INCOMPLETE"),
        "foreign declaration did not produce the explicit migration refusal: {error:#}");
    ensure!(std::fs::read(&manifest_path)? == foreign_bytes
        && std::fs::read(&home_file)? == b"persistent guest home"
        && std::fs::read(&memory_file)? == b"persistent memory",
        "refused migration modified the existing installation");
    Ok(())
}
