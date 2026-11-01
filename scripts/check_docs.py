#!/usr/bin/env python3
"""Validate documentation, archive and index integrity.

This is a documentation-integrity check. It does not build, run or verify the
Shenko product, and it never claims that a requirement is satisfied.
"""
from pathlib import Path
import hashlib
import json
import re
import sys
from urllib.parse import unquote

ROOT = Path(__file__).resolve().parents[1]
errors = []

def check(condition, message):
    if not condition:
        errors.append(message)

def read_json(path):
    return json.loads((ROOT / path).read_text())

def outside_fences(text):
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        match = re.match(r'^(`{3,}|~{3,})', line)
        if match:
            if fence is None:
                fence = (match.group(1)[0], len(match.group(1)))
            elif match.group(1)[0] == fence[0] and len(match.group(1)) >= fence[1]:
                fence = None
            continue
        if fence is None:
            yield number, line

spec = (ROOT / 'SPEC.md').read_text()
spec_bytes = (ROOT / 'SPEC.md').read_bytes()
rows = list(outside_fences(spec))
requirements = read_json('docs/requirements/requirements.json')
behaviors = read_json('docs/requirements/behaviors.json')
sources = read_json('docs/requirements/sources.json')
cases = read_json('docs/requirements/acceptance-cases.json')
coverage = read_json('docs/state/coverage.json')
manifest = read_json('docs/history/manifest.json')

def ids(index, key='id'):
    values = [item[key] for item in index['items']]
    check(len(values) == index['count'], f'{key}: count does not match items')
    check(len(set(values)) == len(values), f'{key}: duplicate definitions')
    return set(values)

rids = ids(requirements)
bids = ids(behaviors)
refids = ids(sources)
cids = ids(cases, 'case_id')
spec_rids = {m.group(1) for _, line in rows if (m := re.match(r'^\| (R\d{3}) \|', line))}
spec_bids = {m.group(1) for _, line in rows if (m := re.match(r'^\| (B\d{2}) ', line))}
spec_refs = {m.group(1) for _, line in rows if (m := re.match(r'^\| (REF-[A-Z0-9-]+) \|', line))}
check(rids == spec_rids, f'SPEC requirement mismatch: {sorted(rids ^ spec_rids)}')
check(bids == spec_bids, f'SPEC behavior mismatch: {sorted(bids ^ spec_bids)}')
check(refids == spec_refs, f'SPEC source mismatch: {sorted(refids ^ spec_refs)}')

pattern = r'^\|\s*(PV\d{2}|A\d{2}|V1-\d{2}|E\d{2}|H\d{2}|K\d{2}|N\d{2}|DX\d{2}|RT\d{2}|P41-\d{2}|UI\d{2}|UI-S\d{2}|QA\d{2}|CFG\d{2}|PG\d{2}|BOOT\d{2}|AW\d{2}|DEP\d{2}|G\d{2})(?=\s|\|)'
defined_cases = [(m.group(1), number, line) for number, line in rows if (m := re.match(pattern, line))]
check(len({x[0] for x in defined_cases}) == len(defined_cases), 'SPEC duplicates acceptance case IDs')
check(cids == {x[0] for x in defined_cases}, 'SPEC acceptance index mismatch')
case_rows = {cid:line for cid, _, line in defined_cases}
for case in cases['items']:
    check(case_rows.get(case['case_id']) == case['source_row'], f"Stale case source row: {case['case_id']}")
for req in requirements['items']:
    check(req['source_row'] in spec.splitlines(), f"Stale requirement source row: {req['id']}")
    check(set(req['behaviors']) <= bids, f"Missing behavior for {req['id']}")
for behavior in behaviors['items']:
    check(set(behavior['requirements']) <= rids, f"Missing requirement for {behavior['id']}")
    used_refs = set(re.findall(r'REF-[A-Z0-9-]+', behavior['refs']))
    check(used_refs <= refids, f"Missing Ref for {behavior['id']}: {sorted(used_refs - refids)}")
covered_ids = [item['requirement_id'] for item in coverage['items']]
check(set(covered_ids) == rids and len(covered_ids) == len(rids), 'Coverage ledger drops or duplicates a requirement')
for item in coverage['items']:
    check(set(item['acceptance_case_ids']) <= cids, f"Unknown case in coverage: {item['requirement_id']}")
    if item['satisfied']:
        check(bool(item['evidence_refs']), f"Satisfied without evidence refs: {item['requirement_id']}")

snapshot = ROOT / manifest['source_snapshot']['file']
check(hashlib.sha256(snapshot.read_bytes()).hexdigest() == manifest['source_snapshot']['sha256'], 'Historical v0.5.1 snapshot changed')
for item in manifest['files']:
    path = ROOT / item['file']
    check(path.exists(), f"Missing archived source: {item['file']}")
    if path.exists():
        check(hashlib.sha256(path.read_bytes()).hexdigest() == item['sha256'], f"Archived source changed: {item['file']}")
check(len(manifest['files']) == manifest['archived_file_count'], 'Archive count mismatch')

old = snapshot.read_text()
old_originals = old.split('# 33 用户需求原文与追加澄清', 1)[1].split('# 34 BoxLite', 1)[0].strip()
new_originals = spec.split('# 33 用户需求原文与追加澄清', 1)[1].split('# 34 BoxLite', 1)[0].strip()
check(old_originals in new_originals, 'Original U01–U49 archive no longer preserved verbatim')
check('## U50 文档仓库与完整开发交接' in new_originals, 'U50 missing')
check('## U51 fork 内自主开发与权限模式澄清' in new_originals, 'U51 missing')
check('## U52 拥有者创建交接仓库' in new_originals, 'U52 missing')
check('## U53 Claude API 2026-10-21 迁移提醒' in new_originals, 'U53 missing')
for number in range(54, 64):
    check(re.search(rf'^## U{number} ', new_originals, re.MULTILINE) is not None,
          f'U{number} missing')
original_case_ids = {item['case_id'] for item in read_json('docs/history/acceptance-source-v0.5.1.json')['cases']}
check(original_case_ids <= cids, 'Original acceptance obligations dropped')

# --- SPEC §48 immutability -----------------------------------------------------
# §1–47 and every original user utterance are immutable bytes. §48 is the only
# permitted append, so the original prefix is verified byte-for-byte and any
# appended text must start with the §48 heading.
spec_integrity = read_json('docs/history/spec-integrity.json')
prefix_bytes = spec_integrity['original_prefix_bytes']
prefix_sha = spec_integrity['original_prefix_sha256']
check(len(spec_bytes) >= prefix_bytes,
      f'SPEC.md is shorter than the immutable original prefix ({len(spec_bytes)} < {prefix_bytes} bytes)')
if len(spec_bytes) >= prefix_bytes:
    actual_prefix = hashlib.sha256(spec_bytes[:prefix_bytes]).hexdigest()
    check(actual_prefix == prefix_sha,
          f'SPEC.md original prefix changed: {actual_prefix} != {prefix_sha}')
    appended = spec_bytes[prefix_bytes:].decode('utf-8', errors='replace')
    if appended:
        check(appended.lstrip('\n').startswith(spec_integrity['appended_section_heading']),
              'SPEC.md may only be extended by the documented §48 appendix')

# --- 2026-11-01 archive ---------------------------------------------------------
archives = read_json('docs/history/archives-20261101.json')
check(len(archives['files']) == archives['archived_file_count'], 'Archive manifest count mismatch')
archived_by_original = {}
for item in archives['files']:
    path = ROOT / item['archived_path']
    check(path.exists(), f"Archived file missing: {item['archived_path']}")
    if path.exists():
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        check(digest == item['sha256'], f"Archived file changed: {item['archived_path']}")
        check(path.stat().st_size == item['bytes'], f"Archived file size changed: {item['archived_path']}")
    archived_by_original[item['original_path']] = item
for retired in archives['retired_root_paths']:
    retired_path = ROOT / retired
    check(not retired_path.exists() and not retired_path.is_symlink(),
          f'Retired path still present at its old location: {retired}')
    check(retired in archived_by_original, f'Retired path is not covered by the archive manifest: {retired}')
for root_entry in archives['retired_root_paths']:
    if root_entry.count('/') != 0:
        continue
    check(not (ROOT / root_entry).exists(), f'Root compatibility entry still present: {root_entry}')

# --- domain specification registry ----------------------------------------------
registry = read_json('docs/domain-specs.json')
registered = {}
for item in registry['specs']:
    registered[item['file']] = item
    check(item['file'].startswith('docs/specs/'), f"Domain spec outside docs/specs/: {item['file']}")
    check(bool(item.get('owner')), f"Domain spec without owner: {item['file']}")
    check(bool(item.get('sections')), f"Domain spec without SPEC sections: {item['file']}")
specs_dir = ROOT / 'docs/specs'
present_specs = sorted(p.name for p in specs_dir.glob('*.md')) if specs_dir.is_dir() else []
for name in present_specs:
    check(f'docs/specs/{name}' in registered, f'Unregistered domain spec file: docs/specs/{name}')
    text = (specs_dir / name).read_text()
    check(text.strip().startswith('#'), f'Domain spec does not start with a heading: docs/specs/{name}')
pending_specs = sorted(
    path for path, item in registered.items() if not (ROOT / item['file']).exists()
)

# --- development Skills and the shared entrypoint --------------------------------
claude_entry = ROOT / 'CLAUDE.md'
check(claude_entry.is_symlink() and claude_entry.readlink() == Path('AGENTS.md'),
      'CLAUDE.md must be a symbolic link to AGENTS.md')
skill_root = ROOT / '.agents/skills'
required_skills = {'shenko-dev'} | {
    'shenko-dev-' + suffix for suffix in (
        'trace-intent', 'inspect-runtime', 'check-upstream', 'change',
        'debug', 'verify', 'integrate', 'checkpoint')
}
skill_dirs = {p.parent.name: p.parent for p in skill_root.glob('*/SKILL.md')}
check(required_skills <= skill_dirs.keys(), 'Missing required development Skill')
for name, directory in skill_dirs.items():
    text = (directory / 'SKILL.md').read_text()
    frontmatter = re.match(r'^---\n(.*?)\n---(?:\n|$)', text, re.DOTALL)
    check(frontmatter is not None, f'Missing Skill frontmatter: {name}')
    if frontmatter:
        fields = frontmatter.group(1)
        check(re.search(rf'^name: {re.escape(name)}$', fields, re.MULTILINE) is not None,
              f'Skill name does not match directory: {name}')
        check(re.search(r'^description: .+\S.*$', fields, re.MULTILINE) is not None,
              f'Skill description missing: {name}')
    check(re.fullmatch(r'[a-z0-9]+(?:-[a-z0-9]+)*', name) is not None and len(name) <= 64,
          f'Invalid Skill name: {name}')
    alias = ROOT / '.claude/skills' / name
    check(alias.is_symlink() and alias.resolve() == directory.resolve(),
          f'Claude Skill must link to its canonical directory: {name}')
check('.agents/skills/shenko-dev/SKILL.md' in (ROOT / 'AGENTS.md').read_text(),
      'AGENTS.md does not identify the mandatory philosophy Skill')

# --- local links ----------------------------------------------------------------
# Historical material keeps its original layout. A link inside SPEC.md to a
# retired root entrypoint is explained by §48.5 and is therefore resolved
# against the archive manifest instead of being reported as broken.
def archived_for(original_path):
    for item in archives['files']:
        if item['original_path'] == original_path:
            return item
    return None

for path in ROOT.rglob('*.md'):
    rel = path.relative_to(ROOT)
    if rel.parts[:2] == ('docs','history') or rel.parts[:3] == ('docs','research','archive'):
        continue
    for _, line in outside_fences(path.read_text()):
        for target in re.findall(r'\]\(([^\s)]+)(?:\s+"[^"]*")?\)', line):
            if re.match(r'^[a-zA-Z][a-zA-Z0-9+.-]*:', target) or target.startswith('#'):
                continue
            local = unquote(target.split('#',1)[0])
            if not local:
                continue
            if (path.parent / local).exists():
                continue
            # A domain spec that another writer still owes is tracked, not broken.
            resolved = (path.parent / local).resolve()
            if resolved.is_relative_to(ROOT):
                relative = resolved.relative_to(ROOT).as_posix()
                if relative in registered:
                    continue
            # SPEC §1–47 keeps its original links; §48.5 explains them.
            if rel == Path('SPEC.md'):
                archived = archived_for(resolved.relative_to(ROOT).as_posix()) \
                    if resolved.is_relative_to(ROOT) else None
                if archived is not None and (ROOT / archived['archived_path']).exists():
                    continue
            check(False, f'Broken local link in {rel}: {target}')

result = {
    'kind': 'documentation_integrity_only',
    'product_tested': False,
    'requirements': len(rids),
    'behaviors': len(bids),
    'source_records': len(refids),
    'acceptance_definitions': len(cids),
    'archived_sources': len(manifest['files']),
    'archived_entrypoints': len(archives['files']),
    'development_skills': len(skill_dirs),
    'domain_specs_registered': len(registered),
    'domain_specs_present': len(present_specs),
    'domain_specs_pending': pending_specs,
    'errors': errors,
}
print(json.dumps(result, ensure_ascii=False, indent=2))
sys.exit(1 if errors else 0)
