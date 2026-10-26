//! Installable AwarenessUnit packages — SPEC §45.1–45.2, §44.7.
//!
//! A minimal package is one `AWARENESS.md` carrying identity, capability card
//! and the intuition body in the same file. Units are installed, enabled,
//! disabled and uninstalled independently; there is no single awareness plugin.
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};

/// Maximum number of bytes of one intuition body returned by
/// [`AwarenessRegistry::active_intuitions`].
pub const MAX_INTUITION_BYTES: usize = 2048;

/// Total bytes of intuition text a step may carry. Beyond it, the remaining
/// units are named (id + capability card) so the model knows they exist.
pub const MAX_TOTAL_INTUITION_BYTES: usize = 4096;

/// One independently installable awareness unit — SPEC §45.1–45.2.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AwarenessUnit {
    pub id: String,
    pub version: String,
    /// Resolved package/artifact reference for the installed content.
    pub package_ref: String,
    /// Path of the `AWARENESS.md` entry that was parsed.
    pub entry_path: PathBuf,
    /// Capability card: what this unit helps the subject notice.
    pub capability_card: String,
    /// Where the intuition came from (SPEC section, user experience, ...).
    pub provenance: Vec<String>,
    /// Other awareness/perception/Skill/code units this one reuses.
    pub dependencies: Vec<String>,
    pub enabled: bool,
    pub installed_at: DateTime<Utc>,
    /// Intuition body, used as the retrieval payload.
    pub body: String,
}

/// Parse an `AWARENESS.md` (or any file with the same minimal frontmatter).
///
/// Frontmatter is delimited by `---` lines and holds simple `key: value` or
/// `key: [a, b]` entries. Everything after the closing delimiter is the
/// intuition body. No YAML crate is required for the minimal package form.
pub fn parse_awareness_md(path: impl AsRef<Path>) -> Result<AwarenessUnit> {
    let path = path.as_ref();
    let content = fs::read_to_string(path)
        .with_context(|| format!("reading awareness entry {}", path.display()))?;
    parse_awareness_md_str(&content, path)
}

/// Parse awareness content already in memory (used for embedded seeds).
pub fn parse_awareness_md_str(content: &str, entry_path: &Path) -> Result<AwarenessUnit> {
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let (frontmatter, body) = split_frontmatter(content, entry_path)?;

    let id = frontmatter
        .get("id")
        .cloned()
        .with_context(|| format!("{} has no `id` in frontmatter", entry_path.display()))?;
    let version = frontmatter
        .get("version")
        .cloned()
        .with_context(|| format!("{} has no `version` in frontmatter", entry_path.display()))?;

    let provenance = list_field(&frontmatter, &["provenance", "provenance_refs"]);
    let dependencies = list_field(&frontmatter, &["dependencies", "dependency_refs"]);

    let capability_card = frontmatter
        .get("capability_card")
        .cloned()
        .or_else(|| first_heading(&body))
        .unwrap_or_else(|| id.clone());

    let package_ref = frontmatter
        .get("package_ref")
        .cloned()
        .unwrap_or_else(|| format!("awareness/{id}@{version}"));

    if body.is_empty() {
        bail!(
            "{} has no intuition body after frontmatter",
            entry_path.display()
        );
    }

    Ok(AwarenessUnit {
        id,
        version,
        package_ref,
        entry_path: entry_path.to_path_buf(),
        capability_card,
        provenance,
        dependencies,
        enabled: true,
        installed_at: Utc::now(),
        body,
    })
}

/// Split `content` into normalized frontmatter keys and the trimmed body.
fn split_frontmatter(
    content: &str,
    entry_path: &Path,
) -> Result<(HashMap<String, String>, String)> {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        // No frontmatter: everything is body, identity must then be absent and
        // parse_awareness_md_str reports the missing `id`.
        return Ok((HashMap::new(), content.trim().to_string()));
    }

    let mut frontmatter = HashMap::new();
    let mut body_lines = Vec::new();
    let mut closed = false;
    for line in lines {
        if !closed {
            let trimmed = line.trim();
            if trimmed == "---" {
                closed = true;
                continue;
            }
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let Some((key, value)) = trimmed.split_once(':') else {
                bail!(
                    "{}: invalid frontmatter line (expected `key: value`): {trimmed}",
                    entry_path.display()
                );
            };
            frontmatter.insert(normalize_key(key), unquote(value.trim()));
        } else {
            body_lines.push(line);
        }
    }
    if !closed {
        bail!(
            "{}: unterminated frontmatter (missing closing `---`)",
            entry_path.display()
        );
    }

    Ok((frontmatter, body_lines.join("\n").trim().to_string()))
}

fn normalize_key(key: &str) -> String {
    key.trim().to_ascii_lowercase().replace('-', "_")
}

fn unquote(value: &str) -> String {
    let value = value.trim();
    for quote in ['"', '\''] {
        if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
            return value[1..value.len() - 1].to_string();
        }
    }
    value.to_string()
}

/// Read a scalar or `[a, b]` frontmatter field as a list.
fn list_field(frontmatter: &HashMap<String, String>, keys: &[&str]) -> Vec<String> {
    let Some(raw) = keys.iter().find_map(|k| frontmatter.get(*k)) else {
        return Vec::new();
    };
    let raw = raw.trim();
    let inner = raw
        .strip_prefix('[')
        .and_then(|v| v.strip_suffix(']'))
        .unwrap_or(raw);
    inner
        .split(',')
        .map(unquote)
        .map(|item| item.trim().to_string())
        .filter(|item| !item.is_empty())
        .collect()
}

fn first_heading(body: &str) -> Option<String> {
    body.lines()
        .find_map(|line| line.trim().strip_prefix('#').map(str::trim))
        .filter(|heading| !heading.is_empty())
        .map(str::to_string)
}

/// Registry of installed awareness units, keyed by unit id.
#[derive(Default)]
pub struct AwarenessRegistry {
    units: RwLock<HashMap<String, AwarenessUnit>>,
}

impl AwarenessRegistry {
    pub fn new() -> Self {
        Self {
            units: RwLock::new(HashMap::new()),
        }
    }

    /// Install the units shipped with Shenko as embedded content.
    ///
    /// `include_str!` keeps this independent of the runtime working directory
    /// or an installed package path.
    pub fn with_seeds() -> Self {
        const SEEDS: &[(&str, &str)] = &[
            (
                "seed-awareness/network-change/AWARENESS.md",
                include_str!("../seed-awareness/network-change/AWARENESS.md"),
            ),
            (
                "seed-awareness/capability-gap/AWARENESS.md",
                include_str!("../seed-awareness/capability-gap/AWARENESS.md"),
            ),
            (
                "seed-awareness/general-principles/AWARENESS.md",
                include_str!("../seed-awareness/general-principles/AWARENESS.md"),
            ),
        ];

        let registry = Self::new();
        let mut units = registry.units.write();
        for (path, content) in SEEDS {
            let unit = parse_awareness_md_str(content, Path::new(path))
                .expect("embedded awareness seed must parse");
            units.insert(unit.id.clone(), unit);
        }
        drop(units);
        registry
    }

    /// Scan `dir/*/AWARENESS.md`, parse each package and register it.
    ///
    /// Id collisions are rejected with the version already installed. Nothing
    /// is inserted unless every parsed unit is unique, both within the scan and
    /// against the registry.
    pub fn install_from_dir(&self, dir: impl AsRef<Path>) -> Result<Vec<String>> {
        let dir = dir.as_ref();
        let mut entry_files = Vec::new();
        for entry in fs::read_dir(dir)
            .with_context(|| format!("scanning awareness packages in {}", dir.display()))?
        {
            let entry = entry?;
            let package_dir = entry.path();
            if !package_dir.is_dir() {
                continue;
            }
            let entry_file = package_dir.join("AWARENESS.md");
            if entry_file.is_file() {
                entry_files.push(entry_file);
            }
        }
        entry_files.sort();

        let mut parsed = Vec::with_capacity(entry_files.len());
        for entry_file in &entry_files {
            parsed.push(parse_awareness_md(entry_file)?);
        }

        let mut units = self.units.write();
        let mut batch_ids = HashSet::new();
        for unit in &parsed {
            if let Some(existing) = units.get(&unit.id) {
                bail!(
                    "awareness unit '{}' already installed at version {}; refusing version {}",
                    unit.id,
                    existing.version,
                    unit.version
                );
            }
            if !batch_ids.insert(unit.id.clone()) {
                bail!(
                    "duplicate awareness unit id '{}' under {}",
                    unit.id,
                    dir.display()
                );
            }
        }

        let installed = parsed.iter().map(|unit| unit.id.clone()).collect();
        for unit in parsed {
            units.insert(unit.id.clone(), unit);
        }
        Ok(installed)
    }

    pub fn enable(&self, id: &str) -> Result<()> {
        let mut units = self.units.write();
        let unit = units
            .get_mut(id)
            .with_context(|| format!("unknown awareness unit '{id}'"))?;
        unit.enabled = true;
        Ok(())
    }

    pub fn disable(&self, id: &str) -> Result<()> {
        let mut units = self.units.write();
        let unit = units
            .get_mut(id)
            .with_context(|| format!("unknown awareness unit '{id}'"))?;
        unit.enabled = false;
        Ok(())
    }

    pub fn uninstall(&self, id: &str) -> Result<AwarenessUnit> {
        self.units
            .write()
            .remove(id)
            .with_context(|| format!("unknown awareness unit '{id}'"))
    }

    /// All known units, ordered by id.
    pub fn list(&self) -> Vec<AwarenessUnit> {
        let mut units: Vec<_> = self.units.read().values().cloned().collect();
        units.sort_by(|a, b| a.id.cmp(&b.id));
        units
    }

    pub fn get(&self, id: &str) -> Option<AwarenessUnit> {
        self.units.read().get(id).cloned()
    }

    /// Every enabled unit's perspective, bounded, for one subject step.
    ///
    /// There is deliberately no keyword gate: a message describing a symptom
    /// ("什么都打不开了") does not contain the word its unit is named after
    /// ("网络"), so matching words would silently withhold exactly the
    /// perspective that was supposed to help. Selection is the model's
    /// judgement; this returns installed material in a deterministic order and
    /// degrades to an index when the budget is exceeded, so a large library is
    /// discoverable rather than truncated invisibly.
    pub fn active_intuitions(&self) -> Vec<String> {
        let mut intuitions = Vec::new();
        let mut index = Vec::new();
        let mut used = 0_usize;
        for unit in self.list().into_iter().filter(|unit| unit.enabled) {
            let body = cap_body(&unit.body, MAX_INTUITION_BYTES);
            if used + body.len() > MAX_TOTAL_INTUITION_BYTES {
                index.push(format!("{} — {}", unit.id, unit.capability_card));
                continue;
            }
            used += body.len();
            intuitions.push(body);
        }
        if !index.is_empty() {
            intuitions.push(format!(
                "其余已安装的觉察（需要时可查阅）：{}",
                index.join("；")
            ));
        }
        intuitions
    }
}

/// Cap one body at `max_bytes` without splitting a UTF-8 character.
fn cap_body(body: &str, max_bytes: usize) -> String {
    if body.len() <= max_bytes {
        return body.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !body.is_char_boundary(end) {
        end -= 1;
    }
    body[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seed_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("seed-awareness")
    }

    fn seed_file(name: &str) -> PathBuf {
        seed_dir().join(name).join("AWARENESS.md")
    }

    #[test]
    fn parses_seed_awareness_md() {
        let unit = parse_awareness_md(seed_file("network-change")).unwrap();
        assert_eq!(unit.id, "awareness.network-change");
        assert_eq!(unit.version, "0.2.0");
        assert_eq!(unit.provenance, vec!["SPEC §9.4".to_string()]);
        assert!(unit.dependencies.is_empty());
        assert!(unit.capability_card.contains("网络"));
        assert!(unit.body.contains("通路"));
        assert!(unit.enabled);
    }

    #[test]
    fn install_enable_disable_list() {
        let registry = AwarenessRegistry::new();
        let installed = registry.install_from_dir(seed_dir()).unwrap();
        assert_eq!(
            installed,
            vec![
                "awareness.capability-gap".to_string(),
                "awareness.general-principles".to_string(),
                "awareness.network-change".to_string()
            ]
        );

        let listed = registry.list();
        assert_eq!(listed.len(), 3);
        assert_eq!(listed[0].id, "awareness.capability-gap");
        assert_eq!(listed[2].id, "awareness.network-change");
        assert!(registry.get("awareness.network-change").unwrap().enabled);

        registry.disable("awareness.network-change").unwrap();
        assert!(!registry.get("awareness.network-change").unwrap().enabled);
        assert!(!registry.active_intuitions().iter().any(|body| body.contains("通路")));
        registry.enable("awareness.network-change").unwrap();
        assert!(registry.get("awareness.network-change").unwrap().enabled);

        assert!(registry.enable("awareness.missing").is_err());
        let removed = registry.uninstall("awareness.capability-gap").unwrap();
        assert_eq!(removed.id, "awareness.capability-gap");
        assert_eq!(registry.list().len(), 2);
    }

    #[test]
    fn active_intuitions_returns_every_enabled_unit_without_a_keyword_gate() {
        let registry = AwarenessRegistry::with_seeds();
        // A symptom message never names the concept; the unit still arrives.
        let intuitions = registry.active_intuitions();
        assert_eq!(intuitions.len(), 3);
        assert!(intuitions.iter().any(|body| body.contains("通路")));
        assert!(intuitions.iter().any(|body| body.contains("最近一次实际做成")));

        registry.disable("awareness.capability-gap").unwrap();
        let after = registry.active_intuitions();
        assert_eq!(after.len(), 2);
        assert!(!after.iter().any(|body| body.contains("最近一次实际做成")));
    }
}
