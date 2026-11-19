from pathlib import Path
import datetime
import hashlib
import ast
import json
import re
import shutil
import subprocess

ROOT = Path('/workspace/scratch/faaa3ccca531')
REPO = ROOT / 'shenko-update-build-evidence'
BASE = 'b10ca4c4064d7cc5d877250f8de834dc4d0dbea5'
original_tokenizer = ast.parse((REPO / 'scripts/ci/clippy_ratchet.py').read_text())
tokenizer_node = next(node for node in original_tokenizer.body if isinstance(node, ast.FunctionDef) and node.name == 'rust_tokens')
tokenizer_namespace = {'re': re}
exec(compile(ast.Module(body=[tokenizer_node], type_ignores=[]), '<original-rust-tokenizer>', 'exec'), tokenizer_namespace)

def old(path):
    return subprocess.check_output(['git', 'show', BASE + ':' + path], cwd=REPO, text=True)

def macros(text):
    tokens = list(tokenizer_namespace['rust_tokens'](text))
    found = []
    for i, token in enumerate(tokens):
        if token not in {'ensure', 'assert', 'assert_eq', 'assert_ne'} or tokens[i+1:i+3] != ['!', '(']:
            continue
        depth = 0
        for j in range(i+2, len(tokens)):
            if tokens[j] == '(':
                depth += 1
            elif tokens[j] == ')':
                depth -= 1
                if depth == 0:
                    found.append(tokens[i:j+1])
                    break
    return found

files = ['crates/shenko-update/src/build_tests.rs', 'crates/shenko-update/src/handoff_tests.rs']
checks = {}
for name in files:
    before, after = old(name), (REPO / name).read_text()
    a, b = macros(before), macros(after)
    assert a == b, name + ': original assertions changed'
    test_names = lambda text: re.findall(r'(?:async )?fn ([A-Za-z0-9_]+)\(', text)
    # New helper methods are allowed; no original function is removed.
    assert set(test_names(before)) <= set(test_names(after))
    checks[name] = {'original_assertions_preserved': len(a), 'all_original_functions_present': True,
                   'before_sha256': hashlib.sha256(before.encode()).hexdigest(),
                   'after_sha256': hashlib.sha256(after.encode()).hexdigest()}

workflow = '.github/workflows/ci.yml'
new_lines = ['          export SHENKO_PTY_WRITE_EVIDENCE="$GITHUB_WORKSPACE/.ci/evidence/pty-write"\n',
             '          export SHENKO_UPDATE_BUILD_EVIDENCE="$GITHUB_WORKSPACE/.ci/evidence/update-build"\n']
current = (REPO / workflow).read_text()
for line in new_lines:
    assert current.count(line) == 1
    current = current.replace(line, '')
assert current == old(workflow)
for name in ['crates/shenko-update/src/build.rs', 'crates/shenko-update/src/handoff.rs',
             'crates/shenko-update/src/smoke.rs']:
    assert (REPO / name).read_text() == old(name)
    checks[name] = {'production_source_byte_identical': True}
diff = subprocess.run(['git', 'diff', '--check'], cwd=REPO, capture_output=True, text=True)
assert diff.returncode == 0
checks.update(observed_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), source_base=BASE,
    workflow_only_two_evidence_exports=True, new_behavioral_windows=0,
    local_toolchains={tool: shutil.which(tool) for tool in ['cargo', 'rustc', 'rustfmt']},
    runtime_validation=False, diff_check_exit=diff.returncode,
    limitation='Lexical conservation and manual review, not Rust compilation or exporter execution.')
(ROOT / 'update-build-evidence-static.json').write_text(json.dumps(checks, indent=2) + '\n')
print(json.dumps(checks, indent=2))
