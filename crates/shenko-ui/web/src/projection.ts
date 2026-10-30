/**
 * Shenko's stable application-semantic UI boundary (SPEC §21.2).
 *
 * This is deliberately not Proto-UI's private PrototypeIR and is not accepted
 * by upstream compilePrototype. A future compiler adapter must translate this
 * semantic document at this boundary; pages and service DTOs never expose IR.
 */
export const UI_PROJECTION_SCHEMA = 'shenko.ui-projection/v1' as const;
export const UI_INTENT_SCHEMA = 'shenko.ui-intent/v1' as const;

export type ViewName = 'timeline' | 'tasks' | 'memory' | 'config';
export type ReadingState = 'following_latest' | 'reading_history' | 'selecting_or_interacting';
export type ConnectionState = 'connecting' | 'online' | 'reconnecting' | 'offline';
// Durable admission stages — SPEC §41.18: accepted/selected/committed are
// separate fact receipts, never a single progress bar.
export type ReceiptState =
  | 'sending'
  | 'sent'
  | 'accepted'
  | 'selected'
  | 'committed'
  | 'conflict'
  | 'rejected'
  | 'unknown';

export interface TimelinePart {
  role: 'subject';
  content: string;
  message_id: string;
  part_id: string;
  part_revision: string;
  next_chunk_seq: number;
  step: number;
  step_id: string;
  commit_ref: string;
  observed_at: string;
  stream_id?: string;
  request_id?: string;
  attempt_id?: string;
  gap: boolean;
}

export interface SubmissionPayload {
  schema_version: 2;
  type: 'submit';
  submission_id: string;
  message_id: string;
  subject_id: string;
  mode: 'append';
  content: string;
  attachment_refs: [];
  observed_step_id?: string;
}

export interface Submission {
  payload: SubmissionPayload;
  created_at: string;
  receipt: ReceiptState;
  detail?: string;
  event_id?: string;
  accepted_commit?: string;
  selected_step_id?: string;
  committed_at?: string;
  // Correlation between this client's send attempts and the query_replies:
  // a view for an older attempt must not undo a newer send's restored ACK.
  send_epoch: number;
}

export interface IdentityView {
  agent_id?: string;
  revision?: number;
  display_name?: string;
  naming?: string;
  soul?: string;
}

export interface ConfigView {
  model?: string;
  data_dir?: string;
  generation?: string;
  api_key_configured?: boolean;
}

export interface SelfView {
  status?: string;
  identity?: IdentityView;
  subject_id?: string;
  connected_clients?: number;
  step?: number;
  last_outbox_observed_at?: string;
  data_dir?: string;
  model?: string;
  devices?: Array<{ id: string; label?: string; name?: string; online?: boolean; kind?: string }>;
  activities?: unknown[];
  artifacts?: Array<{ id: string; title?: string; kind?: string }>;
}

export interface BuildManifest {
  schema_version: 1;
  upstream: { repository: string; commit: string; lock_sha256: string };
  packages: string[];
  adopted_families: string[];
  compiler: { status: string; runtime_imported: boolean };
}

export interface DisplayState {
  view: ViewName;
  panel_open: boolean;
  open_sections: Record<string, boolean>;
  theme: 'dark' | 'light';
  reading: ReadingState;
  new_count: number;
  scroll_request: number;
  notice?: string;
}

export interface UiState {
  source: 'live' | 'demo';
  connection: ConnectionState;
  reconnect_seconds: number;
  server_version?: string;
  append_available: boolean;
  durable_receipts: boolean;
  committed_replay: boolean;
  subject_id: string | null;
  identity?: IdentityView;
  wanted_identity_revision: number | null;
  step: number | null;
  timeline: TimelinePart[];
  submissions: Submission[];
  draft: string;
  config?: ConfigView;
  config_error: boolean;
  self?: SelfView;
  self_error: boolean;
  tasks?: { available?: boolean; items?: Array<Record<string, unknown>>; note?: string };
  tasks_error: boolean;
  memory?: { items?: Array<Record<string, unknown>>; note?: string };
  memory_error: boolean;
  manifest?: BuildManifest;
  display: DisplayState;
}

export interface UiAction {
  action_id: string;
  kind: string;
  revision: string;
  enabled: boolean;
  payload?: Record<string, unknown>;
}

export interface UiNode {
  node_id: string;
  kind: string;
  props: Record<string, any>;
  a11y: Record<string, string | number | boolean>;
  layout: Record<string, string | number | boolean>;
  action_refs: string[];
  fallback?: string;
  children: UiNode[];
}

export interface UiCapabilities {
  append: boolean;
  steer: boolean;
  attachments: boolean;
  durable_input_receipts: boolean;
  committed_replay: boolean;
}

export interface UiProjection {
  schema_version: typeof UI_PROJECTION_SCHEMA;
  surface_id: string;
  revision: number;
  source: 'live' | 'demo';
  subject_id: string | null;
  identity?: IdentityView;
  connection: ConnectionState;
  nodes: UiNode[];
  capabilities: UiCapabilities;
  actions: Record<string, UiAction>;
  assets: Record<string, { kind: string; ref: string; revision: string }>;
}

export interface UiIntent {
  schema_version: typeof UI_INTENT_SCHEMA;
  surface_id: string;
  node_id: string;
  action_id: string;
  action_revision: string;
  payload: Record<string, unknown>;
}

export interface UiDiagnostic {
  level: 'error' | 'warning' | 'info';
  message: string;
  node_id?: string;
}

/** Optional next-version compiler boundary. No private compiler types leak here. */
export interface UiCompilerAdapter<Artifacts = unknown> {
  compile(projection: UiProjection): Promise<{
    artifacts: Artifacts;
    diagnostics: UiDiagnostic[];
  }>;
}

export interface ProjectionController {
  projection(): UiProjection;
  subscribe(listener: () => void): () => void;
  dispatch(intent: UiIntent): void;
  dispose(): void;
}

export type Dispatch = (intent: UiIntent) => void;

export function node(
  node_id: string,
  kind: string,
  props: Record<string, any> = {},
  children: UiNode[] = [],
  options: Partial<Pick<UiNode, 'a11y' | 'layout' | 'action_refs' | 'fallback'>> = {},
): UiNode {
  return {
    node_id,
    kind,
    props,
    children,
    a11y: options.a11y ?? {},
    layout: options.layout ?? {},
    action_refs: options.action_refs ?? [],
    fallback: options.fallback,
  };
}

export function semanticHash(text: string): string {
  // Synchronous, non-secret action binding only; this is not a content digest.
  let hash = 2166136261;
  for (let index = 0; index < text.length; index += 1) {
    hash ^= text.charCodeAt(index);
    hash = Math.imul(hash, 16777619);
  }
  return (hash >>> 0).toString(36);
}

function addAction(
  actions: Record<string, UiAction>,
  action_id: string,
  kind: string,
  revision: string,
  enabled = true,
  payload?: Record<string, unknown>,
): string {
  actions[action_id] = { action_id, kind, revision, enabled, payload };
  return action_id;
}

function entryNodes(state: UiState, actions: Record<string, UiAction>): UiNode[] {
  const subjectRows = state.timeline.filter((part) => part.content.length > 0).map((part) => {
    const copyAction = addAction(
      actions,
      `part.copy.${part.part_id}`,
      'part.copy',
      `copy:${part.part_revision}`,
      true,
      { content: part.content },
    );
    return node(`part.${part.part_id}`, 'subject_part', { part }, [], {
      action_refs: [copyAction],
      a11y: { label: `第 ${part.step} 轮已提交内容` },
      fallback: 'raw_text',
    });
  });
  const submissionRows = state.submissions.map((submission) => {
    const refs: string[] = [];
    if (submission.receipt === 'rejected' || submission.receipt === 'unknown') {
      refs.push(
        addAction(
          actions,
          `submission.retry.${submission.payload.submission_id}`,
          'submission.retry',
          `retry:${submission.payload.submission_id}:${semanticHash(submission.payload.content)}:append`,
          state.connection === 'online',
          { submission_id: submission.payload.submission_id },
        ),
      );
    }
    return node(`submission.${submission.payload.submission_id}`, 'user_submission', { submission }, [], {
      action_refs: refs,
      a11y: { label: '你的追加输入' },
      fallback: 'raw_text',
    });
  });
  return [...subjectRows, ...submissionRows].sort((left, right) => {
    const leftTime = left.kind === 'subject_part' ? left.props.part.observed_at : left.props.submission.created_at;
    const rightTime = right.kind === 'subject_part' ? right.props.part.observed_at : right.props.submission.created_at;
    // Committed history retains replay order for equal timestamps; a later
    // committed reply stays after local input instead of jumping above it.
    return Date.parse(leftTime) - Date.parse(rightTime);
  });
}

export function buildProjection(state: UiState, revision: number): UiProjection {
  const actions: Record<string, UiAction> = {};
  const navNodes: UiNode[] = (['timeline', 'tasks', 'memory', 'config'] as ViewName[]).map((view) => {
    const action = addAction(actions, `view.${view}`, 'view.select', 'view-v1', true, { view });
    return node(`nav.${view}`, 'navigation_item', { view, current: state.display.view === view }, [], {
      action_refs: [action],
      a11y: { current: state.display.view === view ? 'page' : false },
    });
  });
  const timelineChildren = entryNodes(state, actions);
  const timeline = node(
    'timeline',
    'timeline',
    {
      entries: timelineChildren,
      reading: state.display.reading,
      new_count: state.display.new_count,
      scroll_request: state.display.scroll_request,
      hidden: state.display.view !== 'timeline',
      empty: timelineChildren.length === 0,
      connection: state.connection,
      step: state.step,
      subject_status: state.self?.status ?? 'unknown',
    },
    timelineChildren,
    {
      action_refs: [
        addAction(actions, 'timeline.reading', 'timeline.reading', 'display-v1'),
        addAction(actions, 'timeline.latest', 'timeline.latest', 'display-v1'),
      ],
      a11y: { label: '主体时间线' },
      layout: { hidden: state.display.view !== 'timeline' },
      fallback: 'committed_text',
    },
  );
  const draft = state.draft;
  const canAppend =
    state.source === 'live' &&
    state.connection === 'online' &&
    state.append_available &&
    state.durable_receipts &&
    Boolean(state.subject_id) &&
    draft.trim().length > 0;
  const appendAction = addAction(
    actions,
    'composer.append',
    'composer.append',
    `append:${state.subject_id ?? 'unbound'}:${semanticHash(draft.trim())}`,
    canAppend,
  );
  const draftAction = addAction(actions, 'composer.draft', 'composer.draft', 'display-v1', state.source === 'live');
  const composer = node(
    'composer',
    'composer',
    {
      draft,
      connection: state.connection,
      hidden: state.display.view !== 'timeline',
      demo: state.source === 'demo',
      subject_bound: Boolean(state.subject_id),
      append_action: appendAction,
      append_enabled: canAppend,
    },
    [],
    {
      action_refs: [draftAction, appendAction],
      a11y: { label: '向同一主体追加输入' },
      layout: { hidden: state.display.view !== 'timeline' },
    },
  );
  const panelAction = addAction(actions, 'panel.toggle', 'panel.toggle', 'display-v1');
  const themeAction = addAction(actions, 'theme.toggle', 'theme.toggle', 'display-v1');
  const sectionNodes = ['status', 'activity', 'artifacts', 'devices'].map((section) => {
    const action = addAction(actions, `section.${section}`, 'section.toggle', 'display-v1', true, { section });
    return node(`section.${section}`, 'collapsible_section', {
      section,
      open: state.display.open_sections[section] === true,
    }, [], { action_refs: [action] });
  });
  const panel = node('context', 'context_panel', {
    open: state.display.panel_open,
    identity: state.identity,
    self: state.self,
    config: state.config,
    connection: state.connection,
    step: state.step,
    sections: sectionNodes,
    timeline: state.timeline,
  }, sectionNodes, {
    action_refs: [panelAction],
    a11y: { label: '主体详情', modal: state.display.panel_open },
    layout: { open: state.display.panel_open },
  });

  const projection: UiProjection = {
    schema_version: UI_PROJECTION_SCHEMA,
    surface_id: state.source === 'demo' ? 'shenko.main.demo' : 'shenko.main.live',
    revision,
    source: state.source,
    subject_id: state.subject_id,
    identity: state.identity,
    connection: state.connection,
    nodes: [
      node('navigation', 'navigation', { items: navNodes, theme: state.display.theme }, navNodes, {
        action_refs: [panelAction, themeAction],
        a11y: { label: '主要导航' },
      }),
      timeline,
      node('tasks', 'tasks_surface', {
        hidden: state.display.view !== 'tasks',
        data: state.tasks,
        error: state.tasks_error,
      }, [], { layout: { hidden: state.display.view !== 'tasks' }, fallback: 'read_only_list' }),
      node('memory', 'memory_surface', {
        hidden: state.display.view !== 'memory',
        data: state.memory,
        error: state.memory_error,
      }, [], { layout: { hidden: state.display.view !== 'memory' }, fallback: 'read_only_list' }),
      node('config', 'config_surface', {
        hidden: state.display.view !== 'config',
        config: state.config,
        config_error: state.config_error,
        self: state.self,
        self_error: state.self_error,
        connection: state.connection,
        reconnect_seconds: state.reconnect_seconds,
        server_version: state.server_version,
        manifest: state.manifest,
        step: state.step,
        capabilities: {
          append: state.append_available,
          steer: false,
          attachments: false,
          durable_input_receipts: state.durable_receipts,
          committed_replay: state.committed_replay,
        },
      }, [], { layout: { hidden: state.display.view !== 'config' }, fallback: 'read_only_values' }),
      composer,
      panel,
      node('notice', 'status_notice', { text: state.display.notice, source: state.source }, [], {
        a11y: { live: 'polite' },
      }),
    ],
    capabilities: {
      append: state.append_available,
      steer: false,
      attachments: false,
      durable_input_receipts: state.durable_receipts,
      committed_replay: state.committed_replay,
    },
    actions,
    assets: {
      proto_ui: {
        kind: 'compiled_runtime_and_generated_styles',
        ref: '/assets/build-manifest.json',
        revision: state.manifest?.upstream.commit ?? 'unread',
      },
    },
  };
  return projection;
}

export function intentFor(
  projection: UiProjection,
  node_id: string,
  action_id: string,
  payload: Record<string, unknown> = {},
): UiIntent {
  const action = projection.actions[action_id];
  if (!action) throw new Error(`Unknown UI action: ${action_id}`);
  return {
    schema_version: UI_INTENT_SCHEMA,
    surface_id: projection.surface_id,
    node_id,
    action_id,
    action_revision: action.revision,
    payload,
  };
}
