import * as React from 'react';
import { Icon } from './icons';

/**
 * The committed-text renderer (docs/specs/web-interface.md#spec-21-11).
 *
 * Blocks are keyed by kind and ordinal, never by text length, so new chunks at
 * the tail only touch the tail block: stable paragraphs, lists and code
 * figures above keep their DOM, selection and horizontal scroll. An unclosed
 * code fence renders as a code block from its first line on (it never flips
 * between plain text and code) and says it is still open.
 */
interface MarkdownBlock {
  kind: 'code' | 'h1' | 'h2' | 'h3' | 'h4' | 'ul' | 'ol' | 'quote' | 'rule' | 'paragraph';
  key: string;
  text: string;
  language?: string;
  open?: boolean;
  items?: string[];
  /** The first ordinal of an ordered list, as the text numbered it. */
  start?: number;
}

const FENCE = /^\s*```/;
const HEADING = /^(#{1,4})\s+(.*)$/;
const BULLET = /^\s*[-*+]\s+/;
const ORDERED = /^\s*\d+[.)]\s+/;
const RULE = /^\s*([-*_])(\s*\1){2,}\s*$/;

function startsBlock(line: string): boolean {
  return FENCE.test(line) || HEADING.test(line) || BULLET.test(line) || ORDERED.test(line)
    || line.startsWith('>') || RULE.test(line);
}

export function parseMarkdown(text: string): MarkdownBlock[] {
  const lines = text.replaceAll('\r\n', '\n').split('\n');
  const blocks: MarkdownBlock[] = [];
  let index = 0;
  let sequence = 0;
  const nextKey = (kind: string) => `${kind}-${sequence++}`;
  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) { index += 1; continue; }
    if (FENCE.test(line)) {
      const language = line.trimStart().slice(3).trim();
      const key = nextKey('code');
      index += 1;
      const code: string[] = [];
      while (index < lines.length && !FENCE.test(lines[index])) {
        code.push(lines[index]);
        index += 1;
      }
      const open = index >= lines.length;
      if (!open) index += 1;
      blocks.push({ kind: 'code', key, text: code.join('\n'), language, open });
      continue;
    }
    if (RULE.test(line)) {
      blocks.push({ kind: 'rule', key: nextKey('rule'), text: '' });
      index += 1;
      continue;
    }
    const heading = HEADING.exec(line);
    if (heading) {
      blocks.push({ kind: `h${heading[1].length}` as MarkdownBlock['kind'], key: nextKey('heading'), text: heading[2] });
      index += 1;
      continue;
    }
    if (BULLET.test(line) || ORDERED.test(line)) {
      const ordered = ORDERED.test(line);
      const pattern = ordered ? ORDERED : BULLET;
      const key = nextKey('list');
      const items: string[] = [];
      const start = ordered ? Number.parseInt(line.trim(), 10) : undefined;
      while (index < lines.length && pattern.test(lines[index])) {
        items.push(lines[index].replace(pattern, ''));
        index += 1;
      }
      blocks.push({ kind: ordered ? 'ol' : 'ul', key, text: '', items, start: Number.isFinite(start) ? start : undefined });
      continue;
    }
    if (line.startsWith('>')) {
      const key = nextKey('quote');
      const quote: string[] = [];
      while (index < lines.length && lines[index].startsWith('>')) {
        quote.push(lines[index].replace(/^>\s?/, ''));
        index += 1;
      }
      blocks.push({ kind: 'quote', key, text: quote.join('\n') });
      continue;
    }
    const key = nextKey('paragraph');
    const paragraph: string[] = [];
    while (index < lines.length && lines[index].trim() && !startsBlock(lines[index])) {
      paragraph.push(lines[index]);
      index += 1;
    }
    blocks.push({ kind: 'paragraph', key, text: paragraph.join('\n') });
  }
  return blocks;
}

const INLINE = /(`[^`\n]+`|\*\*[^*\n]+\*\*|\[[^\]\n]+\]\((?:https?:\/\/|\/)[^)\s]+\))/g;

/** Inline code, strong text and http(s)/same-origin links; everything else is literal. */
export function inlineText(text: string): React.ReactNode[] {
  return text.split(INLINE).filter(Boolean).map((part, index) => {
    if (part.startsWith('`') && part.endsWith('`') && part.length > 2) {
      return <code className="md-code" key={index}>{part.slice(1, -1)}</code>;
    }
    if (part.startsWith('**') && part.endsWith('**') && part.length > 4) {
      return <strong key={index}>{part.slice(2, -2)}</strong>;
    }
    const link = /^\[([^\]]+)\]\(([^)]+)\)$/.exec(part);
    if (link) {
      return <a key={index} href={link[2]} target="_blank" rel="noreferrer noopener">{link[1]}</a>;
    }
    return <React.Fragment key={index}>{part}</React.Fragment>;
  });
}

export function Markdown({ text }: { text: string }) {
  const blocks = React.useMemo(() => parseMarkdown(text), [text]);
  return <div className="md">{blocks.map((block) => {
    switch (block.kind) {
      case 'code':
        return <CodeBlock key={block.key} code={block.text} language={block.language} open={block.open === true} />;
      case 'ul':
        return <ul key={block.key}>{block.items?.map((item, index) => <li key={index}>{inlineText(item)}</li>)}</ul>;
      case 'ol':
        // The list keeps the number the text started it at, so "3." stays 3.
        return <ol key={block.key} start={block.start}>{block.items?.map((item, index) => <li key={index}>{inlineText(item)}</li>)}</ol>;
      case 'quote':
        return <blockquote key={block.key}>{inlineText(block.text)}</blockquote>;
      case 'rule':
        return <hr key={block.key} />;
      case 'paragraph':
        return <p key={block.key}>{inlineText(block.text)}</p>;
      default: {
        const level = Math.min(4, Number(block.kind.slice(1))) as 1 | 2 | 3 | 4;
        return React.createElement(`h${level + 1}`, { key: block.key }, inlineText(block.text));
      }
    }
  })}</div>;
}

/**
 * A code figure keeps one container for its whole life: the header, font and
 * copy control do not move as lines arrive. Copy returns exactly the visible
 * text of this revision; an open fence is labelled partial.
 */
export function CodeBlock({ code, language, open = false }: { code: string; language?: string; open?: boolean }) {
  const [copied, setCopied] = React.useState(false);
  React.useEffect(() => {
    if (!copied) return;
    const timer = window.setTimeout(() => setCopied(false), 1600);
    return () => window.clearTimeout(timer);
  }, [copied]);
  return (
    <figure className="code" data-open={open ? 'true' : 'false'}>
      <figcaption className="code-head">
        <span className="code-lang">{language || 'text'}</span>
        {open && <span className="code-open">未闭合 · 内容可能未完</span>}
        <button
          type="button"
          className="code-copy"
          onClick={() => {
            void navigator.clipboard?.writeText(code).then(() => setCopied(true), () => setCopied(false));
          }}
          aria-label={open ? '复制当前可见的部分代码' : '复制代码'}
        >
          <Icon name={copied ? 'check' : 'copy'} size={15} />
          <span>{copied ? '已复制' : '复制'}</span>
        </button>
      </figcaption>
      <pre tabIndex={0}><code>{code}</code></pre>
    </figure>
  );
}
