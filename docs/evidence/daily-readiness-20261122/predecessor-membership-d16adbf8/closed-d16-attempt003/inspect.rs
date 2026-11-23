//! Read-only use of the product's existing CAS closure reader on a closed run.
//! No Store open, SQL connection, root publication, repair or effect execution.
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};
use shenko_store::cas::ContentStore;
use std::path::PathBuf;

fn read(cas: &ContentStore, reference: &Value) -> Result<Value> {
    let digest = serde_json::from_value(reference["digest"].clone())?;
    let raw = cas.read_object(&digest)?;
    ensure!(raw.len() as u64 == reference["bytes"].as_u64().context("object length")?);
    Ok(serde_json::from_slice(&raw)?)
}

fn tree(cas: &ContentStore, root: &Value) -> Result<Vec<Value>> {
    let mut queue = vec![root.clone()];
    let mut entries = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    while let Some(reference) = queue.pop() {
        ensure!(seen.insert(reference["digest"].as_str().context("digest")?.to_owned()));
        let page = read(cas, &reference)?;
        entries.extend(page["entries"].as_array().context("entries")?.iter().cloned());
        for edge in page["children"].as_array().context("children")? {
            queue.push(edge["child"].clone());
        }
    }
    Ok(entries)
}

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let base = PathBuf::from(args.next().context("existing CAS directory")?);
    let reference: Value = serde_json::from_str(&args.next().context("original predecessor ObjectRef")?)?;
    ensure!(args.next().is_none());
    let cas = ContentStore::open_existing(base)?;
    let root = read(&cas, &reference)?;
    let entries = tree(&cas, &root["roots"])?;
    ensure!(entries.len() as u64 == root["root_count"].as_u64().context("root count")?);
    let mut orders = vec![("original_tree_read_order", entries.iter().map(|r| r["value"].clone()).collect::<Vec<_>>())];
    let mut reverse = orders[0].1.clone();
    reverse.reverse();
    orders.push(("reversed_original_order", reverse));
    let mut sorted = orders[0].1.clone();
    sorted.sort_by_cached_key(|v| serde_json::to_string(v).unwrap());
    orders.push(("sorted_typed_references", sorted));
    let mut observations = Vec::new();
    for (name, values) in orders {
        let roots = values.into_iter().map(serde_json::from_value).collect::<Result<Vec<_>, _>>()?;
        let started = std::time::Instant::now();
        let closure = shenko_store::roots::object_closure(&cas, roots);
        observations.push(json!({
            "order": name, "elapsed_seconds": started.elapsed().as_secs_f64(),
            "objects": closure.digests, "object_count": closure.digests.len(),
            "missing": closure.missing,
        }));
    }
    println!("{}", serde_json::to_string_pretty(&json!({
        "scope": "Read-only product object_closure over original closed CAS. No Store open, SQL, new model, repaired object, publication or replay.",
        "predecessor": reference, "expected_object_count": root["object_count"],
        "root_count": root["root_count"], "original_root_entries": entries,
        "observations": observations,
    }))?);
    Ok(())
}
