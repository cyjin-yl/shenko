import type { UiNode } from './projection';

/**
 * The configuration panel — SPEC §43.1.
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
  return <div className="config-entries-block">
    <div className="config-entries-head">
      <h3>配置命名空间</h3>
      <p>保存的 desired revision、各消费者实际采用的 revision、以及消费者现在运行的值分别记录；保存成功不等于服务健康，未执行或不可读都如实显示。</p>
    </div>
    {node.props.entries_error === true
      ? <p className="config-entry-note is-error">配置接口不可用</p>
      : rows.length === 0
        ? <p className="config-entry-note">当前没有注册的配置命名空间</p>
        : rows.map((row) => {
          const adoptions = Array.isArray(row.adoptions) ? row.adoptions : [];
          return <div className="config-entry" key={row.key}>
            <div className="config-entry-title">
              <strong>{row.key}</strong>
              <span>{row.application ?? '生效方式未知'}</span>
            </div>
            <div className="config-entry-facts">
              <span>已保存 revision：{row.desired_revision === undefined ? '无' : row.desired_revision}</span>
              <span>consumer 实际运行：{shown(row.current_value)}</span>
            </div>
            {row.current_value_error && <p className="config-entry-note is-error">{row.current_value_error}</p>}
            <div className="config-entry-adoptions">
              {adoptions.length === 0
                ? <span>尚无 consumer adoption</span>
                : adoptions.map((adoption) => <span key={adoption.consumer_id ?? 'unknown'}>
                  {adoption.consumer_id ?? '未知 consumer'} · revision {adoption.adopted_revision ?? '未知'}
                </span>)}
            </div>
            {row.error
              ? <p className="config-entry-note is-error">{row.code ? `${row.code}: ` : ''}{row.error}</p>
              : <div className="config-entry-editor">
                <label>
                  <span>新值（JSON）</span>
                  <input
                    className="config-entry-input"
                    type="text"
                    value={row.draft ?? ''}
                    spellCheck={false}
                    onChange={(event) => emit('config', 'config.draft', { entry: row.key, value: event.target.value })}
                  />
                </label>
                <button
                  className="config-entry-apply"
                  type="button"
                  disabled={!live || !row.apply_action}
                  onClick={() => { if (row.apply_action) emit('config', row.apply_action); }}
                >提交修改</button>
              </div>}
          </div>;
        })}
  </div>;
}
