"""Contract tests for stable identities; run with the declared CI dependencies."""

from pathlib import Path
import tempfile
import unittest

from doc_refs import ContractDocument, ReferenceError, ReferenceResolver


class StableReferenceTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)

    def put(self, path, text):
        (self.root / path).write_text(text, encoding='utf-8')

    def test_case_identity_has_one_explicit_owner_and_survives_inserted_prose(self):
        row = '| ZZ01 | a new case family | the original obligation |'
        text = '<a id="contract"></a>\n| Case | scenario | result |\n| --- | --- | --- |\n' + row
        before = ContractDocument(text).resolve('cases', 'ZZ01', 'contract')
        after = ContractDocument('Unrelated inserted prose.\n\n' + text).resolve('cases', 'ZZ01', 'contract')
        self.assertEqual(before, after)
        with self.assertRaisesRegex(ReferenceError, 'belongs to'):
            ContractDocument('<a id="other"></a>\n' + text).resolve('cases', 'ZZ01', 'other')
        with self.assertRaisesRegex(ReferenceError, '0 definitions'):
            ContractDocument(text).resolve('cases', 'ZZ01', 'absent')
        with self.assertRaisesRegex(ReferenceError, '2 definitions'):
            ContractDocument(text + '\n' + row).resolve('cases', 'ZZ01', 'contract')
        with self.assertRaisesRegex(ReferenceError, '2 definitions'):
            ContractDocument(text + '\n<a id="contract"></a>').resolve('cases', 'ZZ01', 'contract')
        self.assertFalse(ContractDocument('````\n' + text + '\n```\n' + row + '\n````').rows)

    def test_rust_symbols_are_parsed_and_fully_qualified(self):
        text = '''pub struct Owner { native: () }
impl Owner {
    pub fn native(&self) {}
    pub fn dispatch(&self) {
        let mut raw = (1, 2);
        inspect(&raw);
        let _field = &raw.0;
        let _const_pointer = &raw const raw;
        let _mut_pointer = &raw mut raw;
        unsafe extern "C" { fn nested_foreign(value: i32) -> i32; }
    }
}
fn inspect(_: &(i32, i32)) {}
mod nested { pub fn dispatch() {} }
unsafe extern "C" { fn foreign_call(value: i32) -> i32; }
unsafe extern "system" { fn system_call(value: i32) -> i32; }
// fn absent() {} is only a comment.
const TEXT: &str = "fn absent() {}";
'''
        self.put('owner.rs', text)
        resolver = ReferenceResolver(self.root)
        resolver.resolve('owner.rs#Owner.native')
        resolver.resolve('owner.rs#Owner::native')
        resolver.resolve('owner.rs#Owner::dispatch')
        resolver.resolve('owner.rs#nested::dispatch')
        resolver.resolve('owner.rs#Owner::dispatch::nested_foreign')
        resolver.resolve('owner.rs#foreign_call')
        resolver.resolve('owner.rs#system_call')
        self.put('owner.rs', '// unrelated inserted line\n' + text)
        ReferenceResolver(self.root).resolve('owner.rs#Owner::dispatch')
        with self.assertRaisesRegex(ReferenceError, 'missing or unsupported'):
            resolver.resolve('owner.rs#absent')
        with self.assertRaisesRegex(ReferenceError, 'missing or unsupported'):
            resolver.resolve('owner.rs#dispatch')
        with self.assertRaisesRegex(ReferenceError, 'missing or unsupported'):
            resolver.resolve('owner.rs#nested_foreign')
        self.put('ambiguous.rs', '#[cfg(unix)] fn same() {}\n#[cfg(windows)] fn same() {}')
        with self.assertRaisesRegex(ReferenceError, 'ambiguous'):
            resolver.resolve('ambiguous.rs#same')
        self.put('broken.rs', 'pub fn broken( {')
        with self.assertRaisesRegex(ReferenceError, 'unsupported'):
            resolver.resolve('broken.rs#broken')
        self.put('broken_extern.rs', 'pub fn intact() {}\nunsafe extern "C" { fn broken() }')
        with self.assertRaisesRegex(ReferenceError, 'unsupported'):
            resolver.resolve('broken_extern.rs#intact')

    def test_tsx_declarations_and_javascript_tests_keep_semantic_names(self):
        self.put('view.tsx', 'export function View() { return <main/>; }\nclass Owner { render() { return <View/>; } }')
        self.put('view.test.mjs', "import test from 'node:test';\ntest('retains the visible owner', () => {});")
        resolver = ReferenceResolver(self.root)
        resolver.resolve('view.tsx#View')
        resolver.resolve('view.tsx#Owner::render')
        resolver.resolve('view.test.mjs#test::retains the visible owner')
        with self.assertRaisesRegex(ReferenceError, 'missing or unsupported'):
            resolver.resolve('view.tsx#main')
        self.put('overload.ts', 'function choose(value: string): string;\nfunction choose(value: number): number;\nfunction choose(value: unknown) { return value; }')
        with self.assertRaisesRegex(ReferenceError, 'ambiguous'):
            resolver.resolve('overload.ts#choose')

    def test_python_uses_ast_and_does_not_resolve_quoted_declarations(self):
        self.put('owner.py', 'class Owner:\n    def dispatch(self):\n        return "def absent(): pass"\n')
        resolver = ReferenceResolver(self.root)
        resolver.resolve('owner.py#Owner::dispatch')
        with self.assertRaisesRegex(ReferenceError, 'missing or unsupported'):
            resolver.resolve('owner.py#absent')

    def test_structured_references_resolve_unique_targets(self):
        self.put('data.json', '{"contract":{"a/b":{"~key":true}}}')
        self.put('entry.md', '---\nid: subject.frame\nkind: awareness\n---\nThe authored body.\n')
        self.put('index.html', '<html><script>theme()</script><script src="/app.js"></script><div id="app"></div></html>')
        self.put('app.css', ':root { --palette: white; }\n:root { --spacing: 1rem; }')
        resolver = ReferenceResolver(self.root)
        self.assertTrue(resolver.resolve('data.json#/contract/a~1b/~0key'))
        resolver.resolve('entry.md#unit:subject.frame')
        resolver.resolve('index.html#id:app')
        resolver.resolve('index.html#script:inline')
        resolver.resolve('app.css#selector::root::property:--palette')
        with self.assertRaisesRegex(ReferenceError, 'ambiguous'):
            resolver.resolve('app.css#selector::root')
        with self.assertRaisesRegex(ReferenceError, 'JSON pointer target missing'):
            resolver.resolve('data.json#/contract/missing')
        self.put('duplicate.json', '{"contract":1,"contract":2}')
        with self.assertRaisesRegex(ReferenceError, 'ambiguous JSON member'):
            resolver.resolve('duplicate.json#/contract')
        self.put('duplicate.md', '---\nid: first\nid: second\n---\nbody\n')
        with self.assertRaisesRegex(ReferenceError, '2 definitions'):
            resolver.resolve('duplicate.md#unit:second')

    def test_unsupported_and_file_only_references_fail_without_fallback(self):
        self.put('owner.rs', 'pub struct Owner;')
        self.put('unknown.txt', 'Owner')
        resolver = ReferenceResolver(self.root)
        with self.assertRaisesRegex(ReferenceError, 'explicit identity'):
            resolver.resolve('owner.rs')
        with self.assertRaisesRegex(ReferenceError, 'explicit identity'):
            resolver.resolve('owner.rs:1')
        with self.assertRaisesRegex(ReferenceError, 'unsupported reference language'):
            resolver.resolve('unknown.txt#Owner')
        with self.assertRaisesRegex(ReferenceError, 'missing or outside'):
            resolver.resolve('missing.rs#Owner')
        with self.assertRaisesRegex(ReferenceError, 'invalid repository path'):
            resolver.resolve('../owner.rs#Owner')


if __name__ == '__main__':
    unittest.main()
