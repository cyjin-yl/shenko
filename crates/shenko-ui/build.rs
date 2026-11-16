use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::PathBuf;
use sha2::{Digest as _, Sha256};

fn main() -> Result<(), Box<dyn Error>> {
    let root = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").ok_or("missing crate root")?);
    let assets = root.join("src/static");
    let manifest_path = assets.join("build-manifest.json");
    println!("cargo:rerun-if-changed={}", manifest_path.display());
    let manifest: serde_json::Value = serde_json::from_slice(&fs::read(&manifest_path)?)?;
    let artifacts = manifest["artifacts"].as_object().ok_or("asset manifest lacks artifacts")?;
    let mut source = String::from("const BROWSER_MODULES: &[(&str, &str)] = &[\n");
    // The browser build owns module membership. No directory scan can admit
    // an old generation or an unrelated local file into the served product.
    for (name, artifact) in artifacts {
        let Some(filename) = name.strip_prefix("chunks/") else { continue };
        let stem = filename.strip_suffix(".js").ok_or("browser chunk is not JavaScript")?;
        if stem.is_empty() || !stem.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_') {
            return Err("invalid browser chunk name".into());
        }
        let path = assets.join(name);
        let bytes = fs::read(&path)?;
        let digest = format!("{:x}", Sha256::digest(&bytes));
        if artifact["bytes"].as_u64() != Some(bytes.len() as u64)
            || artifact["sha256"].as_str() != Some(digest.as_str()) {
            return Err(format!("browser chunk bytes differ from manifest: {name}").into());
        }
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(source,
            "    ({filename:?}, include_str!(concat!(env!(\"CARGO_MANIFEST_DIR\"), {relative:?}))),",
            relative = format!("/src/static/{name}"))?;
    }
    source.push_str("];\n");
    let output = PathBuf::from(std::env::var_os("OUT_DIR").ok_or("missing build output")?);
    fs::write(output.join("browser_modules.rs"), source)?;
    Ok(())
}
