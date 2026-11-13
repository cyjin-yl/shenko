"""Resolve current documentation identities without source-line coordinates.

The syntax of a reference is ``repository/path#identity``. Rust, TypeScript,
JavaScript and Python identities are qualified declarations; Node test calls
use ``test::the literal test name``. Markdown uses explicit anchors or authored
frontmatter unit IDs. HTML uses ``id:value``, a unique ``element:tag`` or
``script:inline``. CSS uses ``selector:exact selector`` with an optional
``::property:name`` qualifier, and JSON uses an RFC 6901 pointer. No resolver falls back to a
substring, an existing file, an unqualified suffix or a source-line number.
"""

from collections import Counter, defaultdict
from dataclasses import dataclass
from html.parser import HTMLParser
from pathlib import Path, PurePosixPath
import ast
import json
import re


class ReferenceError(ValueError):
    """A reference is missing, unsupported, malformed or ambiguous."""


def outside_fences(text):
    fence = None
    for number, line in enumerate(text.splitlines(), 1):
        match = re.match(r'^\s{0,3}(`{3,}|~{3,})', line)
        if match:
            if fence is None:
                fence = (match.group(1)[0], len(match.group(1)))
            elif (match.group(1)[0] == fence[0] and len(match.group(1)) >= fence[1]
                  and not line[match.end():].strip()):
                fence = None
            continue
        if fence is None:
            yield number, line


def explicit_anchors(line):
    return re.findall(r'<(?:a|span)\s+(?:id|name)=["\']([^"\']+)["\']', line)


def row_identity(line):
    """Read the semantic first cell, never a match elsewhere in the row."""
    match = re.match(r'^\|\s*([^|]+)\|', line)
    if not match:
        return None
    words = match.group(1).strip().split(maxsplit=1)
    if not words:
        return None
    head = words[0]
    if re.fullmatch(r'R\d{3}', head):
        return 'requirements', head
    if re.fullmatch(r'B\d{2}', head):
        return 'behaviors', head
    if re.fullmatch(r'REF-[A-Z0-9-]+', head):
        return 'sources', head
    prefix = head.rstrip('0123456789')
    if (prefix not in {'R', 'B', 'U'} and len(head) - len(prefix) >= 2
            and re.fullmatch(r'[A-Z][A-Z0-9-]*', prefix)):
        return 'cases', head
    return None


def acceptance_header(line):
    cells = [cell.strip() for cell in line.strip().strip('|').split('|')]
    return bool(cells) and (cells[0] == 'Case' or (
        cells[0] in {'ID', '编号'} and cells[-1] in {
            '通过条件', '可判定结果', '必须观察到的行为', '场景与通过条件', '成立条件',
            '必须可观察的结果', '必须观察到的结果', '必须出现的真实证据',
        }))


@dataclass(frozen=True)
class ContractRow:
    kind: str
    identity: str
    anchor: str
    text: str


class ContractDocument:
    """Explicit anchor ownership and semantic table definitions in one document."""

    def __init__(self, text):
        self.anchors = Counter()
        self.rows = defaultdict(list)
        anchor = ''
        previous = None
        in_acceptance = None
        for _, line in outside_fences(text):
            for name in explicit_anchors(line):
                self.anchors[name] += 1
                anchor = name
            if not line.startswith('|'):
                previous = None
                in_acceptance = None
                continue
            if re.fullmatch(r'\|[\s|:-]+\|?', line):
                in_acceptance = acceptance_header(previous) if previous else None
                continue
            previous = line
            identity = row_identity(line)
            if identity is None or (identity[0] == 'cases' and in_acceptance is False):
                continue
            kind, name = identity
            self.rows[kind, name].append(ContractRow(kind, name, anchor, line))

    def resolve(self, kind, identity, anchor):
        count = self.anchors[anchor]
        if count != 1:
            raise ReferenceError(f'explicit anchor {anchor!r} has {count} definitions; expected one')
        definitions = self.rows[kind, identity]
        if len(definitions) != 1:
            raise ReferenceError(f'{identity} has {len(definitions)} definitions; expected one')
        row = definitions[0]
        if row.anchor != anchor:
            raise ReferenceError(f'{identity} belongs to #{row.anchor}, not #{anchor}')
        return row


def repository_path(root, relative):
    path = PurePosixPath(relative)
    if (not relative or path.is_absolute() or path.as_posix() != relative
            or any(part in {'..', '.'} for part in relative.split('/'))):
        raise ReferenceError(f'invalid repository path: {relative!r}')
    resolved = (root / relative).resolve()
    if not resolved.is_relative_to(root.resolve()) or not resolved.is_file():
        raise ReferenceError(f'reference target is missing or outside the repository: {relative}')
    return resolved


def _one(symbols, identity):
    count = symbols[identity]
    if count == 0:
        raise ReferenceError(f'missing or unsupported symbol {identity!r}')
    if count != 1:
        raise ReferenceError(f'ambiguous symbol {identity!r}: {count} definitions; qualify its owner')


def _python_symbols(text):
    symbols = Counter()

    def walk(nodes, owners):
        for node in nodes:
            if isinstance(node, (ast.FunctionDef, ast.AsyncFunctionDef, ast.ClassDef)):
                name = owners + [node.name]
                symbols['::'.join(name)] += 1
                walk(node.body, name)
            else:
                if isinstance(node, (ast.Assign, ast.AnnAssign)):
                    targets = node.targets if isinstance(node, ast.Assign) else [node.target]
                    for target in targets:
                        if isinstance(target, ast.Name):
                            symbols['::'.join(owners + [target.id])] += 1
                # Conditional declarations retain their lexical owner. Repeated
                # alternatives stay ambiguous; no execution/config is guessed.
                for field in ('body', 'orelse', 'finalbody'):
                    children = getattr(node, field, None)
                    if isinstance(children, list):
                        walk(children, owners)
                for handler in getattr(node, 'handlers', []):
                    walk(handler.body, owners)

    try:
        walk(ast.parse(text).body, [])
    except SyntaxError as error:
        raise ReferenceError(f'Python syntax is unsupported: {error.msg}') from error
    return symbols


def _tree_symbols(text, suffix):
    try:
        from tree_sitter import Language, Parser
        if suffix == '.rs':
            import tree_sitter_rust
            language = Language(tree_sitter_rust.language())
        elif suffix in {'.ts', '.tsx'}:
            import tree_sitter_typescript
            language = Language(tree_sitter_typescript.language_tsx() if suffix == '.tsx'
                                else tree_sitter_typescript.language_typescript())
        else:
            import tree_sitter_javascript
            language = Language(tree_sitter_javascript.language())
    except ImportError as error:
        raise ReferenceError('symbol parser unavailable; install scripts/requirements-ci.txt') from error
    source = text.encode('utf-8')
    tree = Parser(language).parse(source)
    if tree.root_node.has_error:
        raise ReferenceError('source has syntax unsupported by the pinned language grammar')
    symbols = Counter()

    def spelling(node):
        return source[node.start_byte:node.end_byte].decode('utf-8') if node else ''

    def type_name(node):
        if node is None:
            return ''
        if node.type == 'generic_type':
            return type_name(node.child_by_field_name('type'))
        return ''.join(spelling(node).split())

    def walk(node, owners):
        child_owners = owners
        if suffix == '.rs' and node.type == 'impl_item':
            owner = type_name(node.child_by_field_name('type'))
            trait = type_name(node.child_by_field_name('trait'))
            if not owner:
                raise ReferenceError('unsupported Rust implementation owner')
            child_owners = owners + [f'<{owner} as {trait}>' if trait else owner]
        else:
            declarations = ({'function_item', 'function_signature_item', 'struct_item',
                             'enum_item', 'trait_item', 'type_item', 'const_item',
                             'static_item', 'mod_item', 'macro_definition',
                             'field_declaration', 'enum_variant'} if suffix == '.rs' else
                            {'function_declaration', 'function_signature', 'generator_function_declaration',
                             'class_declaration', 'abstract_class_declaration',
                             'interface_declaration', 'type_alias_declaration',
                             'enum_declaration', 'method_definition', 'method_signature',
                             'public_field_definition', 'property_signature',
                             'variable_declarator', 'internal_module'})
            if node.type in declarations:
                name_node = node.child_by_field_name('name')
                if name_node is not None and name_node.type in {
                        'identifier', 'type_identifier', 'field_identifier',
                        'property_identifier', 'private_property_identifier'}:
                    name = spelling(name_node)
                    child_owners = owners + [name]
                    symbols['::'.join(child_owners)] += 1
            # Node's native test API has semantic names even when the callback
            # is anonymous. Only literal names and the actual test callee shape
            # are supported, not arbitrary calls or source-text matches.
            if suffix in {'.js', '.mjs', '.ts', '.tsx'} and node.type == 'call_expression':
                callee = spelling(node.child_by_field_name('function'))
                arguments = node.child_by_field_name('arguments')
                if callee in {'test', 'it', 'describe'} and arguments and arguments.named_children:
                    label = arguments.named_children[0]
                    if label.type == 'string':
                        parts = [part for part in label.named_children if part.type == 'string_fragment']
                        if len(parts) == 1 and len(label.named_children) == 1:
                            name = f'{callee}::{spelling(parts[0])}'
                            symbols['::'.join(owners + [name])] += 1
                            if callee == 'describe':
                                child_owners = owners + [name]
        for child in node.named_children:
            walk(child, child_owners)

    walk(tree.root_node, [])
    return symbols


class _HtmlSymbols(HTMLParser):
    def __init__(self):
        super().__init__(convert_charrefs=True)
        self.symbols = Counter()

    def handle_starttag(self, tag, attrs):
        self.symbols['element:' + tag] += 1
        if tag == 'script' and not any(name == 'src' for name, _ in attrs):
            self.symbols['script:inline'] += 1
        for name, value in attrs:
            if name == 'id' and value is not None:
                self.symbols['id:' + value] += 1

    handle_startendtag = handle_starttag


def _css_symbols(text):
    try:
        import tinycss2
    except ImportError as error:
        raise ReferenceError('CSS parser unavailable; install scripts/requirements-ci.txt') from error
    symbols = Counter()

    def walk(rules):
        for rule in rules:
            if rule.type == 'error':
                raise ReferenceError(f'unsupported CSS syntax: {rule.message}')
            if rule.type == 'qualified-rule':
                selector = tinycss2.serialize(rule.prelude).strip()
                symbols['selector:' + selector] += 1
                declarations = tinycss2.parse_declaration_list(
                    rule.content, skip_comments=True, skip_whitespace=True)
                for declaration in declarations:
                    if declaration.type == 'declaration':
                        symbols[f'selector:{selector}::property:{declaration.name}'] += 1
            elif rule.type == 'at-rule' and rule.content is not None and rule.lower_at_keyword in {
                    'media', 'supports', 'layer', 'container', 'scope', 'document', 'starting-style'}:
                walk(tinycss2.parse_rule_list(rule.content, skip_comments=True, skip_whitespace=True))

    walk(tinycss2.parse_stylesheet(text, skip_comments=True, skip_whitespace=True))
    return symbols


class ReferenceResolver:
    def __init__(self, root):
        self.root = Path(root)
        self.cache = {}

    def resolve(self, reference):
        if not isinstance(reference, str):
            raise ReferenceError('reference must be a path#identity string')
        relative, separator, identity = reference.partition('#')
        if not separator or not identity:
            raise ReferenceError(f'reference needs an explicit identity: {reference!r}')
        path = repository_path(self.root, relative)
        suffix = path.suffix
        if relative not in self.cache:
            text = path.read_text(encoding='utf-8')
            if suffix == '.json':
                def unique_object(pairs):
                    value = {}
                    for key, item in pairs:
                        if key in value:
                            raise ReferenceError(f'ambiguous JSON member {key!r}')
                        value[key] = item
                    return value
                self.cache[relative] = json.loads(text, object_pairs_hook=unique_object)
            elif suffix == '.py':
                self.cache[relative] = _python_symbols(text)
            elif suffix in {'.rs', '.ts', '.tsx', '.js', '.mjs'}:
                self.cache[relative] = _tree_symbols(text, suffix)
            elif suffix == '.html':
                parser = _HtmlSymbols()
                parser.feed(text)
                self.cache[relative] = parser.symbols
            elif suffix == '.css':
                self.cache[relative] = _css_symbols(text)
            elif suffix == '.md':
                # Authored cognition entries already have a stable structured
                # identity in YAML frontmatter, without an HTML anchor.
                if text.startswith('---\n'):
                    try:
                        import yaml
                    except ImportError as error:
                        raise ReferenceError('YAML parser unavailable; install scripts/requirements-ci.txt') from error
                    header, boundary, _ = text[4:].partition('\n---\n')
                    if not boundary:
                        raise ReferenceError('unterminated Markdown frontmatter')
                    try:
                        metadata = yaml.compose(header)
                    except yaml.YAMLError as error:
                        raise ReferenceError(f'unsupported YAML frontmatter: {error}') from error
                    if not isinstance(metadata, yaml.MappingNode):
                        raise ReferenceError('Markdown frontmatter is not a mapping')
                    identities = [value for key, value in metadata.value if key.value == 'id']
                    if len(identities) != 1:
                        raise ReferenceError(f'Markdown frontmatter id has {len(identities)} definitions')
                    value = identities[0]
                    if not isinstance(value, yaml.ScalarNode) or value.tag != 'tag:yaml.org,2002:str':
                        raise ReferenceError('Markdown frontmatter has no string id')
                    self.cache[relative] = Counter({'unit:' + value.value: 1})
                else:
                    self.cache[relative] = Counter(anchor for _, line in outside_fences(text)
                                                   for anchor in explicit_anchors(line))
            else:
                raise ReferenceError(f'unsupported reference language: {suffix or "(none)"}')
        symbols = self.cache[relative]
        if suffix == '.json':
            if not identity.startswith('/'):
                raise ReferenceError('structured-data references require a nonempty RFC 6901 pointer')
            value = symbols
            for escaped in identity[1:].split('/'):
                if re.search(r'~(?![01])', escaped):
                    raise ReferenceError(f'invalid JSON pointer token: {escaped!r}')
                token = escaped.replace('~1', '/').replace('~0', '~')
                if isinstance(value, dict) and token in value:
                    value = value[token]
                elif (isinstance(value, list) and re.fullmatch(r'0|[1-9][0-9]*', token)
                      and int(token) < len(value)):
                    value = value[int(token)]
                else:
                    raise ReferenceError(f'JSON pointer target missing: #{identity}')
            return value
        _one(symbols, identity)
