import type { UiNode } from './projection';
import { Button, ProtoInput } from './ui-kit';

/**
 * The configuration panel — docs/specs/configuration-playground.md#spec-43-1.
 *
 * Every row is a fact the service actually reported: the saved desired
 * revision, the value each consumer adopted, and the value a consumer runs
 * now. Editing submits the same durable change the subject's `config.manage`
 * tool submits; the panel holds no second configuration, and an unavailable
 * or unreadable key is rendered as unavailable instead of as a default.
 */
interface ConfigEntryRow {
  key: string;
  namespace: string;
  logical_key: string;
  application?: string;
  desired_revision?: number;
  desired_value?: unknown;
  current_value?: unknown;
  current_value_error?: string;
  adoptions?: Array<{ consumer_id?: string; adopted_revision?: number; detail?: string }>;
  draft?: string;
  apply_action?: string;
  code?: string;
  error?: string;
}

type Emit = (nodeId: string, actionId: string, payload?: Record<string, unknown>) => void;

function shown(value: unknown): string {
  if (value === undefined) return '未知';
  if (value === null) return 'null';
  if (typeof value === 'string') return value;
  return JSON.stringify(value);
}

export function ConfigEntries({ node, emit }: { node: UiNode; emit: Emit }) {
  const rows = (Array.isArray(node.props.entries) ? node.props.entries : []) as ConfigEntryRow[];
  const live = node.props.live === true;
  if (node.props.entries_error === true) return <p className="muted is-error">配置接口不可用。</p>;
  if (rows.length === 0) return <p className="muted">当前没有注册的配置项。</p>;
  return <ul className="config-entries">{rows.map((row) => {
    const adoptions = Array.isArray(row.adoptions) ? row.adoptions : [];
    const labelId = `config-${row.key.replace(/[^a-zA-Z0-9_-]/g, '-')}`;
    return <li className="config-entry" key={row.key}>
      <div className="config-entry-head">
        <code className="config-key" id={labelId}>{row.key}</code>
        <span className="muted">{row.application ?? '生效方式未知'}</span>
      </div>
      {/* Saved, adopted and running are three facts; saving is not health. */}
      <dl className="facts is-compact">
        <div className="fact"><dt>已保存</dt><dd>{row.desired_revision === undefined ? '无' : `revision ${row.desired_revision}`}</dd></div>
        <div className="fact"><dt>实际运行</dt><dd className="is-mono">{shown(row.current_value)}</dd></div>
        <div className="fact"><dt>采用</dt><dd>{adoptions.length === 0
          ? '尚无 consumer 采用'
          : adoptions.map((adoption) => `${adoption.consumer_id ?? '未知 consumer'} · r${adoption.adopted_revision ?? '?'}`).join('；')}</dd></div>
      </dl>
      {row.current_value_error && <p className="muted is-error">{row.current_value_error}</p>}
      {row.error
        ? <p className="muted is-error">{row.code ? `${row.code}: ` : ''}{row.error}</p>
        : <div className="field-row">
          <ProtoInput
            className="input is-mono"
            labelledBy={labelId}
            value={row.draft ?? ''}
            placeholder="新值（JSON）"
            onValueChange={(detail: { value: string }) => emit('config', 'config.draft', { entry: row.key, value: detail.value })}
          />
          <Button
            variant="outline"
            size="sm"
            disabled={!live || !row.apply_action}
            onClick={() => { if (row.apply_action) emit('config', row.apply_action); }}
          >提交修改</Button>
        </div>}
    </li>;
  })}</ul>;
}
