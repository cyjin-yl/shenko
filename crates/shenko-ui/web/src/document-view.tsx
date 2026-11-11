/**
 * The rich layer of a semantic document: real components, not a debug view.
 *
 * docs/specs/configuration-playground.md#spec-57-6, docs/specs/web-interface.md#spec-21-6: a `rich` document is rendered by real components and
 * degraded by the surface's own capabilities (docs/specs/web-interface.md#spec-21-7). The renderer therefore
 * knows the document's node vocabulary (the kinds the server validates) and
 * renders each kind as an element with its own semantics.
 *
 * Two rules hold everywhere here:
 *
 * * An unknown kind or a missing prop degrades to something READABLE (the
 *   node's own text, or a named placeholder) — never to a JSON dump, and
 *   never to a crash that takes the committed text beside it down.
 * * An action is the server's action: the node's `action_id` is looked up in
 *   the document's own option list, and the click submits that option. The
 *   client never invents an action identity (docs/specs/web-interface.md#spec-21-8).
 */
import React from 'react';

export interface DocumentActionBinding {
  action_id: string;
  revision?: string;
  nonce?: string;
  payload?: unknown;
}

export interface DocumentNodeView {
  node_id: string;
  kind: string;
  props?: Record<string, unknown>;
  children?: DocumentNodeView[];
  action?: DocumentActionBinding | null;
}

function isObject(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null && !Array.isArray(value);
}

function text(value: unknown): string {
  if (typeof value === 'string') return value;
  if (typeof value === 'number' || typeof value === 'boolean') return String(value);
  return '';
}

/** One option of a select or a form field, as the document authored it. */
interface Choice {
  value: string;
  label: string;
  description?: string;
}

function choices(value: unknown): Choice[] {
  if (!Array.isArray(value)) return [];
  return value
    .map((entry) => {
      if (isObject(entry)) {
        const label = text(entry.label) || text(entry.text) || text(entry.value);
        const description = text(entry.description);
        return {
          value: text(entry.value) || label,
          label,
          description: description || undefined,
        };
      }
      const label = text(entry);
      return { value: label, label };
    })
    .filter((choice) => choice.label !== '' || choice.value !== '');
}

interface Field {
  name: string;
  label: string;
  type: string;
  value: string;
  required: boolean;
  options: Choice[];
}

function fields(value: unknown): Field[] {
  if (!Array.isArray(value)) return [];
  return value
    .map((entry) => {
      if (!isObject(entry)) return null;
      const name = text(entry.name) || text(entry.id);
      const label = text(entry.label) || name;
      if (!name && !label) return null;
      return {
        name,
        label,
        type: text(entry.type) || 'text',
        value: text(entry.value),
        required: entry.required === true,
        options: choices(entry.options),
      };
    })
    .filter((field): field is Field => field !== null);
}

interface SeriesPoint {
  label: string;
  value: number;
}

function series(value: unknown): SeriesPoint[] {
  if (!Array.isArray(value)) return [];
  return value
    .map((entry) => {
      if (isObject(entry)) {
        const raw = entry.value;
        const numeric = typeof raw === 'number' ? raw : Number.parseFloat(text(raw));
        return { label: text(entry.label) || text(entry.name), value: Number.isFinite(numeric) ? numeric : 0 };
      }
      const numeric = Number.parseFloat(text(entry));
      return { label: '', value: Number.isFinite(numeric) ? numeric : 0 };
    })
    .filter((point) => point.label !== '' || point.value !== 0);
}

/** The props of the document node the renderer understands. */
interface NodeProps {
  title: string;
  subtitle: string;
  text: string;
  code: string;
  language: string;
  columns: string[];
  rows: string[][];
  left?: { title?: string; items?: string[]; text?: string };
  right?: { title?: string; items?: string[]; text?: string };
  label: string;
  options: Choice[];
  fields: Field[];
  unit: string;
  series: SeriesPoint[];
  src: string;
  alt: string;
  caption: string;
  mediaKind: string;
  actions: { label: string; action_id?: string }[];
}

function propsOf(node: DocumentNodeView): NodeProps {
  const raw = isObject(node.props) ? node.props : {};
  const left = isObject(raw.left) ? raw.left : undefined;
  const right = isObject(raw.right) ? raw.right : undefined;
  const side = (value: unknown) => (isObject(value)
    ? {
      title: text(value.title),
      items: Array.isArray(value.items)
        ? value.items.map(text).filter((entry) => entry.trim() !== '')
        : [],
      text: text(value.text),
    }
    : undefined);
  return {
    title: text(raw.title),
    subtitle: text(raw.subtitle),
    text: text(raw.text),
    code: text(raw.code),
    language: text(raw.language),
    columns: Array.isArray(raw.columns) ? raw.columns.map(text) : [],
    rows: Array.isArray(raw.rows)
      ? raw.rows
        .map((row) => (Array.isArray(row) ? row.map(text) : [text(row)]))
        .filter((row) => row.length > 0)
      : [],
    left: side(left),
    right: side(right),
    label: text(raw.label),
    options: choices(raw.options),
    fields: fields(raw.fields),
    unit: text(raw.unit),
    series: series(raw.series),
    src: text(raw.src) || text(raw.url),
    alt: text(raw.alt),
    caption: text(raw.caption),
    mediaKind: text(raw.kind),
    actions: Array.isArray(raw.actions)
      ? raw.actions
        .map((entry) => (isObject(entry)
          ? { label: text(entry.label) || text(entry.text), action_id: text(entry.action_id) }
          : { label: text(entry) }))
        .filter((entry) => entry.label !== '')
      : [],
  };
}

export interface RichDocumentProps {
  nodes: DocumentNodeView[];
  /** The document's own options, so a node's action can find its identity. */
  options: { action_id: string; nonce: string; revision: string; label: string }[];
  /** The UI node's action refs, in the same order as the options. */
  actionRefs: string[];
  onAction: (actionRef: string) => void;
  /** The Markdown renderer of the shell, reused so both read the same. */
  renderMarkdown: (text: string) => React.ReactNode;
}

/**
 * The rich document.
 *
 * The node tree is rendered as elements. A node the renderer does not know
 * shows its own text and names the kind it could not render: the reader sees
 * that something was there, and the committed text beside it is untouched.
 */
export function RichDocument({ nodes, options, actionRefs, onAction, renderMarkdown }: RichDocumentProps) {
  const optionIndex = new Map(options.map((option, index) => [option.action_id, index]));
  const actionFor = (node: DocumentNodeView): string | undefined => {
    const actionId = node.action?.action_id;
    if (!actionId) return undefined;
    const index = optionIndex.get(actionId);
    if (index === undefined) return undefined;
    return actionRefs[index];
  };
  const render = (node: DocumentNodeView, depth: number): React.ReactNode => (
    <DocumentNode
      key={node.node_id}
      node={node}
      depth={depth}
      actionRef={actionFor(node)}
      onAction={onAction}
      renderMarkdown={renderMarkdown}
    />
  );
  return <div className="rich-document">{nodes.map((node) => render(node, 0))}</div>;
}

function DocumentNode({
  node,
  depth,
  actionRef,
  onAction,
  renderMarkdown,
}: {
  node: DocumentNodeView;
  depth: number;
  actionRef?: string;
  onAction: (actionRef: string) => void;
  renderMarkdown: (text: string) => React.ReactNode;
}) {
  const props = propsOf(node);
  const children = (node.children ?? []).map((child) => (
    <DocumentNode
      key={child.node_id}
      node={child}
      depth={depth + 1}
      actionRef={undefined}
      onAction={onAction}
      renderMarkdown={renderMarkdown}
    />
  ));
  const withChildren = (element: React.ReactNode) => (
    <>{element}{children.length > 0 ? <div className="document-children">{children}</div> : null}</>
  );

  switch (node.kind) {
    case 'section':
      return withChildren(
        <section className="document-section" data-node-kind="section">
          {props.title ? <h3 className="document-section-title">{props.title}</h3> : null}
          {props.subtitle ? <p className="document-section-subtitle">{props.subtitle}</p> : null}
        </section>,
      );
    case 'text':
      return withChildren(
        <p className="document-text" data-node-kind="text">{props.text}</p>,
      );
    case 'markdown':
      return withChildren(
        <div className="document-markdown" data-node-kind="markdown">{renderMarkdown(props.text)}</div>,
      );
    case 'code':
      return withChildren(
        <figure className="document-code code-block" data-node-kind="code">
          <figcaption>{props.language || 'text'}</figcaption>
          <pre tabIndex={0}><code>{props.code}</code></pre>
        </figure>,
      );
    case 'table':
      return withChildren(
        <div className="document-table-wrap" data-node-kind="table">
          <table className="document-table">
            {props.columns.length > 0 && (
              <thead>
                <tr>{props.columns.map((column, index) => <th key={`${column}-${index}`} scope="col">{column}</th>)}</tr>
              </thead>
            )}
            <tbody>
              {props.rows.map((row, index) => (
                <tr key={`row-${index}`}>
                  {row.map((cell, cellIndex) => <td key={`cell-${cellIndex}`}>{cell}</td>)}
                </tr>
              ))}
            </tbody>
          </table>
        </div>,
      );
    case 'compare':
      return withChildren(
        <div className="document-compare" data-node-kind="compare">
          {[props.left, props.right].map((side, index) => (
            <div className="document-compare-side" key={`side-${index}`}>
              {side?.title ? <h4>{side.title}</h4> : null}
              {side?.text ? <p>{side.text}</p> : null}
              {side?.items && side.items.length > 0 && (
                <ul>{side.items.map((item, itemIndex) => <li key={`item-${itemIndex}`}>{item}</li>)}</ul>
              )}
            </div>
          ))}
        </div>,
      );
    case 'select':
      return withChildren(
        <div className="document-select" data-node-kind="select">
          {props.label ? <p className="document-field-label">{props.label}</p> : null}
          <ul className="document-options-list">
            {props.options.map((choice) => (
              <li key={choice.value}>
                <button
                  type="button"
                  className="document-choice"
                  disabled={!actionRef}
                  onClick={() => actionRef && onAction(actionRef)}
                >
                  <span className="document-choice-label">{choice.label}</span>
                  {choice.description ? <span className="document-choice-description">{choice.description}</span> : null}
                </button>
              </li>
            ))}
          </ul>
        </div>,
      );
    case 'form':
      return withChildren(
        <div className="document-form" data-node-kind="form">
          {props.label ? <p className="document-field-label">{props.label}</p> : null}
          {props.fields.map((field) => (
            <label className="document-field" key={field.name || field.label}>
              <span className="document-field-label">{field.label}</span>
              {field.options.length > 0 ? (
                <select defaultValue={field.value} disabled={!actionRef}>
                  {field.options.map((choice) => <option key={choice.value} value={choice.value}>{choice.label}</option>)}
                </select>
              ) : (
                <input
                  type={field.type === 'number' ? 'number' : field.type === 'checkbox' ? 'checkbox' : 'text'}
                  defaultValue={field.value}
                  required={field.required}
                  readOnly={!actionRef}
                />
              )}
            </label>
          ))}
          {actionRef && (
            <button type="button" className="document-submit" onClick={() => onAction(actionRef)}>提交</button>
          )}
        </div>,
      );
    case 'chart': {
      const max = props.series.reduce((peak, point) => Math.max(peak, Math.abs(point.value)), 0);
      return withChildren(
        <div className="document-chart" data-node-kind="chart">
          {props.title ? <p className="document-field-label">{props.title}</p> : null}
          <ul className="document-chart-list">
            {props.series.map((point, index) => (
              <li key={`${point.label}-${index}`}>
                <span className="document-chart-label">{point.label}</span>
                <span className="document-chart-bar" data-ratio={max > 0 ? Math.abs(point.value) / max : 0} aria-hidden="true" />
                <span className="document-chart-value">{point.value}{props.unit ?? ''}</span>
              </li>
            ))}
          </ul>
        </div>,
      );
    }
    case 'media':
      return withChildren(
        <figure className="document-media" data-node-kind="media">
          {props.mediaKind === 'image' && props.src
            ? <img src={props.src} alt={props.alt} />
            : props.src
              ? <a href={props.src} target="_blank" rel="noreferrer">{props.alt || props.src}</a>
              : <span className="document-media-missing">{props.alt || '媒体'}</span>}
          {props.caption ? <figcaption>{props.caption}</figcaption> : null}
        </figure>,
      );
    case 'actions':
      return withChildren(
        <div className="document-actions" data-node-kind="actions">
          {props.actions.map((action, index) => (
            <button
              type="button"
              key={`${action.label}-${index}`}
              className="document-action"
              disabled={!actionRef}
              onClick={() => actionRef && onAction(actionRef)}
            >
              {action.label}
            </button>
          ))}
        </div>,
      );
    default:
      // An unknown kind is named, not dumped: the reader learns that the
      // document carried something this surface cannot render, and the
      // committed text beside it stays the record.
      return withChildren(
        <div className="document-unknown" data-node-kind={node.kind}>
          <p className="document-unknown-label">这段界面内容（{node.kind}）当前界面还不能呈现。</p>
          {props.text ? <p className="document-unknown-text">{props.text}</p> : null}
        </div>,
      );
  }
}
