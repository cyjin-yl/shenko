import * as React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { RichDocument, type DocumentNodeView } from './document-view';
import {
  intentFor,
  partState,
  type ActionReceiptView,
  type ConnectionState,
  type DegradedDocument,
  type DocumentErrorView,
  type Dispatch,
  type ReadingAnchor,
  type ReadingSelection,
  type Submission,
  type TaskSummary,
  type TimelinePart,
  type UiNode,
  type UiProjection,
  type ViewName,
  type WorkView,
} from './projection';
import { resolveAnchor, selectionApplies } from './reading-state';
import { Icon } from './icons';
import { Markdown } from './markdown';
import {
  Button,
  ProtoTextarea,
  ScrollRoot,
  ScrollScrollbar,
  ScrollThumb,
  ScrollViewport,
  CollapsibleContent,
  CollapsibleRoot,
  CollapsibleTrigger,
  Separator,
  ToggleGroupItem,
  ToggleGroupRoot,
  Toggle,
  TransportBadge,
  connectionAnnouncement,
  dayLabel,
  formatTime,
  type Emit,
} from './ui-kit';
import { ConfigSurface, ContextPanel, MemorySurface, PlaygroundSurface, TasksSurface } from './surfaces';
import { ImView } from './im';
import {
  collectThreads,
  isAttentionTask,
  isDelegation,
  subjectParticipant,
  trajectoryMessages,
  childParticipant,
  demoTrajectory,
  type ConversationRow,
  type ThreadMessage,
  type TrajectoryResponse,
} from './threads';
import {
  lensSnapshot,
  pinnedWorks,
  setWorkPinned,
  isPage,
  subscribeLenses,
  syncWorkLenses,
  type LensContribution,
} from './slots';

export interface ProjectionRenderer {
  render(projection: UiProjection, dispatch: Dispatch): void;
  patch(projection: UiProjection): void;
  dispose(): void;
}

export function createReactProjectionRenderer(host: HTMLElement): ProjectionRenderer {
  const root: Root = createRoot(host);
  let dispatch: Dispatch = () => undefined;
  const patch = (projection: UiProjection) => {
    root.render(<ProjectionView projection={projection} dispatch={dispatch} />);
  };
  return {
    render(projection, nextDispatch) {
      dispatch = nextDispatch;
      patch(projection);
    },
    patch,
    dispose() {
      root.unmount();
    },
  };
}

/** The stream's lens: whichever contribution is open beside it. */
type OpenLens = { kind: 'view'; view: ViewName } | { kind: 'work'; artifact_id: string } | null;

function ProjectionView({ projection, dispatch }: { projection: UiProjection; dispatch: Dispatch }) {
  const nodes = Object.fromEntries(projection.nodes.map((item) => [item.kind, item]));
  const navigation = nodes.navigation as UiNode;
  const timeline = nodes.timeline as UiNode;
  const composer = nodes.composer as UiNode;
  const panel = nodes.context_panel as UiNode;
  const notice = nodes.status_notice as UiNode;
  const emit: Emit = React.useCallback((nodeId, actionId, payload = {}) => {
    dispatch(intentFor(projection, nodeId, actionId, payload));
  }, [projection, dispatch]);

  const theme = navigation.props.theme === 'light' ? 'light' : 'dark';
  React.useEffect(() => {
    document.documentElement.dataset.theme = theme;
    document.documentElement.classList.toggle('dark', theme === 'dark');
    document.documentElement.classList.toggle('light', theme === 'light');
    document.querySelector('meta[name="theme-color"]')?.setAttribute('content', theme === 'light' ? '#fcfcfc' : '#1c1c1c');
  }, [theme]);

  const items = navigation.props.items as UiNode[];
  const current = (items.find((item) => item.props.current)?.props.view ?? 'timeline') as ViewName;
  const name = projection.identity?.display_name || undefined;
  const demo = projection.source === 'demo';
  const lenses = React.useSyncExternalStore(subscribeLenses, lensSnapshot);
  const works = nodes.playground_surface?.props.playground?.artifacts as WorkView[] | undefined;
  const knownWorks = React.useRef<WorkView[]>([]);
  React.useEffect(() => {
    if (works) knownWorks.current = works;
    syncWorkLenses(works);
  }, [works]);

  // A work lens is this page's reading state; the projection only knows that
  // the Playground view is the one beside the stream.
  const [workLens, setWorkLens] = React.useState<string | null>(null);
  const open: OpenLens = workLens && current === 'playground'
    ? { kind: 'work', artifact_id: workLens }
    : current === 'timeline' ? null : { kind: 'view', view: current };
  const navRef = (view: ViewName) => items.find((item) => item.props.view === view);
  const selectView = (view: ViewName) => {
    const item = navRef(view);
    if (item) emit(item.node_id, item.action_refs[0], { view });
  };
  const openLens = (lens: LensContribution | undefined) => {
    if (!lens) {
      setWorkLens(null);
      selectView('timeline');
    } else if (lens.source === 'work' && lens.artifact_id) {
      setWorkLens(lens.artifact_id);
      if (current !== 'playground') selectView('playground');
    } else if (lens.view) {
      setWorkLens(null);
      selectView(lens.view);
    }
  };
  const openId = open === null ? null : open.kind === 'work' ? `work:${open.artifact_id}` : open.view;

  React.useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (!(event.metaKey || event.ctrlKey) || event.altKey || event.shiftKey) return;
      if (event.defaultPrevented || event.composedPath().some((target) => target instanceof HTMLElement
        && (target.isContentEditable || target.matches('input, textarea, select')))) return;
      if (event.key === '0') { event.preventDefault(); openLens(undefined); return; }
      const index = Number(event.key);
      if (!Number.isInteger(index) || index < 1 || index > lenses.length) return;
      event.preventDefault();
      const lens = lenses[index - 1];
      openLens(openId === lens.id ? undefined : lens);
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  });

  const tasks = (nodes.tasks_surface?.props.data?.items ?? []) as TaskSummary[];

  // Two views of the same subject: 对话 is where the owner talks with it and
  // watches the groups it is in; Inspect is its raw stream of thought. The
  // choice is this browser's reading preference.
  const [tab, setTabState] = React.useState<'im' | 'inspect'>(() => {
    const query = new URLSearchParams(location.search).get('tab');
    if (query === 'inspect' || query === 'im') return query;
    try { return localStorage.getItem('shenko.tab.v1') === 'inspect' ? 'inspect' : 'im'; } catch { return 'im'; }
  });
  const setTab = (next: 'im' | 'inspect') => {
    setTabState(next);
    try { localStorage.setItem('shenko.tab.v1', next); } catch { /* page-only preference */ }
  };
  const [threadId, setThreadId] = React.useState<string>(() => new URLSearchParams(location.search).get('thread') ?? 'direct');
  const conversations = useConversations(projection.source === 'live', timeline.props.step as number | null);
  const threads = React.useMemo(
    () => collectThreads({ projection, tasks, conversations: demo ? demoConversations : conversations.rows }),
    [projection, tasks, conversations.rows, demo],
  );
  const inspect = React.useMemo(() => ({
    tasks,
    openThread: (id: string) => { setThreadId(id); setTab('im'); },
    subject: subjectParticipant(projection),
    demo,
  }), [tasks, projection, demo]);
  const openWork = open?.kind === 'work'
    ? (works ?? knownWorks.current).find((work) => work.artifact_id === open.artifact_id)
    : undefined;

  return (
    <div className="shell" data-source={projection.source} data-lens={openId ?? 'none'}>
      <a className="skip-link" href="#composer-editor" onClick={(event) => {
        event.preventDefault();
        document.querySelector<HTMLTextAreaElement>('#composer-editor textarea')?.focus();
      }}>跳到输入区</a>

      <header className="topbar">
        <button
          type="button"
          className="presence"
          onClick={() => emit('context', 'panel.toggle')}
          aria-label={`${name ?? '未命名主体'}，打开主体详情`}
        >
          <span className="presence-name">
            {name ?? <span className="is-unnamed">未命名主体</span>}
            {demo && <span className="tag">模拟投影</span>}
          </span>
          <Attention step={timeline.props.step} status={timeline.props.subject_status} tasks={tasks} />
        </button>
        <nav className="tabs" aria-label="视图">
          <ToggleGroupRoot
            className="tab-switch"
            a11yLabel="视图"
            value={[tab]}
            onValueChange={(detail: { value: readonly string[] }) => {
              const next = detail.value.find((value) => value !== tab);
              if (next === 'im' || next === 'inspect') setTab(next);
            }}
          >
            <ToggleGroupItem value="im" className="tab-item"><Icon name="chat" size={16} />对话</ToggleGroupItem>
            <ToggleGroupItem value="inspect" className="tab-item"><Icon name="timeline" size={16} />Inspect</ToggleGroupItem>
          </ToggleGroupRoot>
        </nav>
        <nav className="lensbar" aria-label="镜头">
          <ToggleGroupRoot
            className="lens-switch"
            a11yLabel="在时间线旁打开"
            value={openId ? [openId] : []}
            onValueChange={(detail: { value: readonly string[] }) => {
              const next = detail.value.find((id) => id !== openId) ?? null;
              openLens(next ? lenses.find((lens) => lens.id === next) : undefined);
            }}
          >
            {lenses.map((lens, index) => (
              <ToggleGroupItem key={lens.id} value={lens.id} className={`lens-item source-${lens.source}`}>
                <Icon name={lens.icon} size={17} />
                <span className="lens-label">{lens.title}</span>
                <span className="sr-only">{index < 9 ? `（Ctrl/⌘ ${index + 1}）` : ''}</span>
              </ToggleGroupItem>
            ))}
          </ToggleGroupRoot>
        </nav>
        <div className="topbar-actions">
          <TransportBadge connection={projection.connection} demo={demo} />
          <Toggle
            active={timeline.props.wrap === true}
            label="长代码行软换行"
            onChange={() => emit('timeline', 'timeline.code_wrap')}
          ><Icon name="wrap" /></Toggle>
          <Button
            variant="ghost"
            size="icon"
            label={theme === 'light' ? '切换到深色' : '切换到浅色'}
            onClick={() => emit('navigation', 'theme.toggle')}
          ><Icon name={theme === 'light' ? 'moon' : 'sun'} /></Button>
          <Button variant="ghost" size="icon" label="主体详情" onClick={() => emit('context', 'panel.toggle')}>
            <Icon name="panel" />
          </Button>
        </div>
      </header>

      <div className="body">
        <main className="stream" id="main" data-tab={tab}>
          {tab === 'im'
            ? <ImView
              threads={threads}
              selected={threadId}
              onSelect={setThreadId}
              composer={<Composer node={composer} emit={emit} variant="im" />}
              renderAttachment={(message: ThreadMessage) => message.node?.kind === 'semantic_document'
                ? <SemanticDocumentView node={message.node} emit={emit} />
                : null}
              emit={emit}
              onOpenInspect={() => setTab('inspect')}
              historyControl={(beforeLoad) => <SubmissionHistory node={timeline} emit={emit} beforeLoad={beforeLoad} />}
              demo={demo}
            />
            : <InspectContext.Provider value={inspect}>
              <TimelineSurface
                node={timeline}
                projection={projection}
                emit={emit}
                onOpenWork={(artifactId) => openLens({ id: `work:${artifactId}`, title: '', icon: 'file', order: 0, source: 'work', artifact_id: artifactId })}
              />
              <Composer node={composer} emit={emit} />
            </InspectContext.Provider>}
          {/* Results of the reader's own acts (copied, rejected stale action…)
              are a quiet status line, not a floating toast. */}
          <div className="statusline" role="status" aria-live="polite">{[notice.props.text, conversations.error].filter(Boolean).join(' · ')}</div>
        </main>
        <aside className="lens" hidden={open === null} aria-label={open === null ? undefined : lenses.find((lens) => lens.id === openId)?.title}>
          <div className="lens-head">
            <span className="lens-title">{lenses.find((lens) => lens.id === openId)?.title ?? openWork?.title ?? ''}</span>
            {open?.kind === 'work' && (
              <span className="lens-sub">作品页 · 主体自己写的扩展，可回退到任一修订</span>
            )}
            <Button variant="ghost" size="icon" label="关闭镜头，回到时间线" onClick={() => openLens(undefined)}>
              <Icon name="close" />
            </Button>
          </div>
          <div className="lens-body">
            {open?.kind === 'work'
              ? <WorkFrame key={open.artifact_id} artifactId={open.artifact_id} title={String(openWork?.title ?? open.artifact_id)} revision={openWork?.revision} activeManifestDigest={openWork?.active_manifest_digest} />
              : <>
                <TasksSurface node={nodes.tasks_surface} />
                <MemorySurface node={nodes.memory_surface} />
                <ConfigSurface node={nodes.config_surface} projection={projection} emit={emit} />
                <PlaygroundSurface
                  node={nodes.playground_surface}
                  onOpenWork={(artifactId) => { setWorkLens(artifactId); }}
                />
              </>}
          </div>
        </aside>
      </div>

      <ContextPanel node={panel} projection={projection} emit={emit} />
      <span className="sr-only" aria-live="polite">{connectionAnnouncement(projection.connection)}</span>
    </div>
  );
}

/**
 * What the subject is doing now, from facts only: running and waiting tasks,
 * the last committed round, the reported status. Nothing is inferred; with no
 * facts the line says so.
 */
function Attention({ step, status, tasks }: { step: number | null; status: string; tasks: TaskSummary[] }) {
  // The subject's own status is shown separately from the work it owns.
  const active = tasks.filter(isAttentionTask);
  const facts: string[] = [];
  if (active.length === 1) facts.push(`在做：${String(active[0].kind ?? active[0].task_id)}`);
  else if (active.length > 1) facts.push(`${active.length} 件事进行中`);
  facts.push(step ? `第 ${step} 轮` : '尚无已提交步骤');
  if (status && status !== 'unknown') facts.push(status);
  return <span className="attention">{facts.join(' · ')}</span>;
}

/**
 * A work's published page, live beside the stream. Works are trusted
 * extensions by default (docs/specs/web-interface.md#5-1): the subject wrote,
 * tested and can roll them back, so the page shares the shell's origin. The
 * safety rope is the named revision and the shell's safe mode (`?safe=1`),
 * which opens no work page. Its revision keys the frame, so a new revision is
 * a fresh page.
 */
function WorkFrame({ artifactId, title, revision, activeManifestDigest }: { artifactId: string; title: string; revision?: number; activeManifestDigest?: string }) {
  const pinned = pinnedWorks().includes(artifactId);
  const [, rerender] = React.useReducer((value: number) => value + 1, 0);
  // A lens shows the adopted page; the authored head is a draft until a
  // publication adopts it, and previewing it is the reader's explicit choice.
  const [adopted, setAdopted] = React.useState<{ state: 'loading' | 'none' | 'ready'; revision?: string }>({ state: 'loading' });
  const [error, setError] = React.useState<string>();
  const [draft, setDraft] = React.useState(false);
  React.useEffect(() => {
    if (SAFE_MODE) return;
    let live = true;
    fetch(`/playground/${encodeURIComponent(artifactId)}/adopted`, { method: 'HEAD', cache: 'no-store' })
      .then((response) => {
        if (!live) return;
        if (!response.ok && response.status !== 404) throw new Error(`HTTP ${response.status}`);
        setError(undefined);
        const next = response.ok
          ? { state: 'ready' as const, revision: response.headers.get('x-artifact-revision') ?? undefined }
          : { state: 'none' as const };
        setAdopted((previous) => previous.state === next.state && previous.revision === next.revision ? previous : next);
      })
      .catch(() => { if (live) setError('作品采纳状态暂不可用'); });
    return () => { live = false; };
  }, [artifactId, revision, activeManifestDigest]);
  if (SAFE_MODE) {
    return <div className="work-frame"><p className="muted safe-note">安全模式：不打开作品页。去掉地址里的 <code>safe=1</code> 即可恢复。</p></div>;
  }
  const src = draft ? `/playground/${artifactId}` : `/playground/${artifactId}/adopted`;
  return (
    <div className="work-frame">
      <div className="work-frame-bar">
        <span className="muted">
          {draft ? `草稿预览 · r${revision ?? '—'}` : adopted.state === 'ready' ? `已采纳 · r${adopted.revision ?? '—'}` : adopted.state === 'none' ? '尚未采纳' : ''}
        </span>
        {draft && adopted.state === 'ready' && <Button variant="ghost" size="sm" onClick={() => setDraft(false)}>回到已采纳版本</Button>}
        {!draft && adopted.state === 'ready' && revision !== undefined && String(revision) !== adopted.revision && (
          <Button variant="ghost" size="sm" onClick={() => setDraft(true)}>预览草稿 r{revision}</Button>
        )}
        <Button variant="ghost" size="sm" onClick={() => { setWorkPinned(artifactId, !pinned); rerender(); }}>
          {pinned ? '从镜头栏取下' : '固定到镜头栏'}
        </Button>
        <a className="work-frame-link" href={src} target="_blank" rel="noreferrer noopener">新窗口</a>
      </div>
      {error && <p className="muted" role="status">{error}</p>}
      {!draft && adopted.state === 'none'
        ? <div className="work-frame-empty">
          <p className="muted">这件作品还没有采纳的版本，镜头只显示已采纳的页面。</p>
          <Button variant="outline" size="sm" onClick={() => setDraft(true)}>预览草稿 r{revision ?? '—'}（未采纳）</Button>
        </div>
        : !draft && adopted.state === 'loading'
          ? <div className="work-frame-empty" />
          : <iframe
            key={`${src}@${draft ? revision ?? 0 : adopted.revision ?? 0}`}
            className="work-iframe"
            src={src}
            title={title}
          />}
    </div>
  );
}

/// Safe mode opens no work page and pins no work lens: the rescue path when a
/// trusted extension misbehaves (docs/specs/web-interface.md#spec-21-3).
const SAFE_MODE = new URLSearchParams(location.search).get('safe') === '1';

/// What Inspect needs beyond the projection: the task list (to find the
/// delegations a round started) and a way to open a thread in 对话.
const InspectContext = React.createContext<{
  tasks: TaskSummary[];
  openThread: (id: string) => void;
  subject: ReturnType<typeof subjectParticipant>;
  demo: boolean;
}>({ tasks: [], openThread: () => undefined, subject: { id: 'subject', name: '主体', role: 'subject' }, demo: false });

/// Channel conversations, re-read when a new step commits.
function useConversations(live: boolean, step: number | null): { rows: ConversationRow[]; error?: string } {
  const [rows, setRows] = React.useState<ConversationRow[]>([]);
  const [error, setError] = React.useState<string>();
  React.useEffect(() => {
    if (!live) return;
    let current = true;
    fetch('/api/conversations', { cache: 'no-store' })
      .then((response) => {
        if (!response.ok) throw new Error(`会话读取失败：HTTP ${response.status}`);
        return response.json();
      })
      .then((data) => {
        if (!current) return;
        if (!data || data.available !== true || !Array.isArray(data.messages)) {
          throw new Error(typeof data?.note === 'string' ? data.note : '会话暂不可用');
        }
        setRows(data.messages);
        setError(undefined);
      })
      .catch((failure) => { if (current) setError(failure instanceof Error ? failure.message : '会话暂不可用'); });
    return () => { current = false; };
  }, [live, step]);
  return { rows, error };
}

function SubmissionHistory({ node, emit, beforeLoad }: { node: UiNode; emit: Emit; beforeLoad?: () => void }) {
  const history = node.props.submission_history;
  if (!history || !history.loading && !history.error && !history.has_older) return null;
  return <div className="im-system">
    {history.loading && <span role="status">正在读取消息历史…</span>}
    {history.error && <span role="status">消息历史暂不可用：{history.error} </span>}
    {!history.loading && (history.has_older || history.error) && <span data-load-submissions="true"><Button
      variant="ghost" size="sm"
      onClick={() => { beforeLoad?.(); emit('timeline', 'submissions.load_older'); }}
    >{history.error ? '重新读取消息历史' : '加载更早的消息'}</Button></span>}
  </div>;
}

const demoConversations: ConversationRow[] = (() => {
  const ago = (minutes: number) => new Date(Date.now() - minutes * 60_000).toISOString();
  const group = { thread_key: 'qq:demo|group|10086', thread_label: '群 · 本地模型研究', thread_kind: 'group', thread_shape: 'group', channel: 'napcat' } as const;
  const p2p = { thread_key: 'feishu:demo|chat|oc_demo', thread_label: '单聊 ou_demo', thread_kind: 'chat', thread_shape: 'one_to_one', channel: 'feishu' } as const;
  return [
    { ...group, direction: 'inbound', message_id: 'dc1', sender_id: 'a', sender_name: '阿青', text: '有人试过 27B 在 4 路 128K 下的吞吐吗', at: ago(50) },
    { ...group, direction: 'inbound', message_id: 'dc2', sender_id: 'b', sender_name: 'Mo', text: '我这边 KV 一满就掉得厉害', at: ago(48) },
    { ...group, direction: 'outbound', message_id: 'dc3', text: '我们这边是自动分 KV，负载对半的时候会落到 50/50。可以先看看是不是启动顺序导致的重试。', at: ago(47), state: 'done' },
    { ...p2p, direction: 'inbound', message_id: 'dp1', sender_id: 'ou_demo', sender_name: '林以', text: '周五的分享你那边能给个十分钟的版本吗？', at: ago(30) },
    { ...p2p, direction: 'outbound', message_id: 'dp2', text: '可以。我先把三个结论压成一页，晚上发你看。', at: ago(28), state: 'done' },
  ];
})();

/// The delegations one committed round started, branching off the spine:
/// each is its own trajectory, readable inline or as a group thread in 对话.
function DelegationBranches({ stepId }: { stepId: string }) {
  const { tasks, openThread, subject, demo } = React.useContext(InspectContext);
  const delegations = tasks.filter((task) => isDelegation(task) && task.created_by_step_id === stepId);
  if (delegations.length === 0) return null;
  return <div className="branches">{delegations.map((task) => (
    <DelegationBranch key={String(task.task_id)} task={task} subject={subject} demo={demo}
      onOpen={() => openThread(`delegation:${String(task.task_id)}`)} />
  ))}</div>;
}

function DelegationBranch({ task, subject, demo, onOpen }: {
  task: Record<string, unknown>; subject: ReturnType<typeof subjectParticipant>; demo: boolean; onOpen: () => void;
}) {
  const id = String(task.task_id);
  const child = childParticipant(id);
  const [open, setOpen] = React.useState(false);
  const [trace, setTrace] = React.useState<ThreadMessage[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  React.useEffect(() => {
    if (!open || demo) return;
    let live = true;
    fetch(`/api/tasks/${encodeURIComponent(id)}/trajectory`, { cache: 'no-store' })
      .then((response) => { if (!response.ok) throw new Error(String(response.status)); return response.json(); })
      .then((data: TrajectoryResponse) => { if (live) setTrace(trajectoryMessages(data, subject)); })
      .catch((reason) => { if (live) setError(String(reason?.message ?? reason)); });
    return () => { live = false; };
  }, [open, id, String(task.state), demo]);
  return (
    <CollapsibleRoot className="branch" open={open} onOpenChange={(detail: { open: boolean }) => setOpen(detail.open)}>
      <div className="branch-head">
        <CollapsibleTrigger className="branch-trigger">
          <Icon name="chevron" size={14} />
          <span className="branch-title">委派 → {child.name}</span>
          <span className="branch-state" data-state={String(task.state)}>{String(task.state)}</span>
        </CollapsibleTrigger>
        <button type="button" className="text-action" onClick={onOpen}><Icon name="chat" size={14} />在对话中打开</button>
      </div>
      <CollapsibleContent className="branch-body">
        {demo ? <ol className="trace">{trajectoryMessages(demoTrajectory(id), subject).map((item) => (
          <li key={item.key} className={`trace-row role-${item.from.role} kind-${item.kind}`}>
            <span className="trace-who">{item.kind === 'system' ? '·' : item.from.name}</span>
            <span className="trace-text">{item.tag ? <b>{item.tag} </b> : null}{item.text || '（无文字）'}</span>
            {item.meta && <span className="trace-meta">{item.meta}</span>}
          </li>
        ))}</ol>
          : error ? <p className="muted is-error">轨迹读取失败：{error}</p>
            : !trace ? <p className="muted">正在读取轨迹…</p>
              : <ol className="trace">{trace.map((item) => (
                <li key={item.key} className={`trace-row role-${item.from.role} kind-${item.kind}`}>
                  <span className="trace-who">{item.kind === 'system' ? '·' : item.from.name}</span>
                  <span className="trace-text">{item.tag ? <b>{item.tag} </b> : null}{item.text || '（无文字）'}</span>
                  {item.meta && <span className="trace-meta">{item.meta}</span>}
                </li>
              ))}</ol>}
      </CollapsibleContent>
    </CollapsibleRoot>
  );
}

interface AnchorSnapshot {
  key?: string;
  offset?: number;
  scrollTop: number;
  index?: number;
  previousKey?: string;
}

interface TimelineSurfaceProps {
  node: UiNode;
  projection: UiProjection;
  emit: Emit;
  /** Opens a page-bearing work as a lens beside the stream. */
  onOpenWork: (artifactId: string) => void;
}

class TimelineSurface extends React.Component<TimelineSurfaceProps, { announcement: string }> {
  /** The bounded announcement for a screen reader (docs/specs/web-interface.md#spec-57-7).
   *
   * It says that committed content arrived and which round it was — it does
   * NOT read the answer again: a reader who is listening should not have the
   * whole answer spoken twice.
   */
  state: { announcement: string } = { announcement: '' };

  private capture = React.createRef<HTMLDivElement>();
  private scrollElement: HTMLElement | null = null;
  private lastAnchor: AnchorSnapshot | null = null;
  private resizeFrame = 0;
  private anchorFrame = 0;
  private selectionTimer: number | undefined;
  private restoredSelection: ReadingSelection | undefined;
  /** The committed entry this surface already announced. */
  private announcedEntry: string | undefined;

  componentDidMount() {
    this.scrollElement = this.findScrollElement();
    if (this.props.node.props.reading === 'following_latest') {
      this.scrollToEnd();
      // The upstream scroll area settles its viewport after first layout.
      this.resizeFrame = requestAnimationFrame(() => {
        this.resizeFrame = 0;
        if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
      });
    } else this.restoreSavedAnchor(this.props.node.props.anchor as ReadingAnchor | undefined);
    this.rememberAnchor();
    this.restoreSavedSelection();
    window.addEventListener('resize', this.onResize);
    window.visualViewport?.addEventListener('resize', this.onResize);
    document.addEventListener('selectionchange', this.onSelectionChange);
  }

  componentWillUnmount() {
    window.removeEventListener('resize', this.onResize);
    window.visualViewport?.removeEventListener('resize', this.onResize);
    document.removeEventListener('selectionchange', this.onSelectionChange);
    if (this.resizeFrame) cancelAnimationFrame(this.resizeFrame);
    if (this.anchorFrame) cancelAnimationFrame(this.anchorFrame);
    clearTimeout(this.selectionTimer);
  }

  getSnapshotBeforeUpdate(): AnchorSnapshot | null {
    return this.captureAnchor();
  }

  componentDidUpdate(_previous: TimelineSurfaceProps, _state: unknown, snapshot: AnchorSnapshot | null) {
    if (!this.scrollElement?.isConnected) this.scrollElement = this.findScrollElement();
    if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
    else if (snapshot) this.restoreAnchor(snapshot);
    this.rememberAnchor();
    this.restoreSavedSelection();
    this.announceCommitted();
  }

  /// Announce newly committed content once, in bounded words.
  private announceCommitted() {
    const entries = (this.props.node.props.entries ?? []) as UiNode[];
    let newest: { key: string; label: string } | undefined;
    for (let index = entries.length - 1; index >= 0 && !newest; index -= 1) {
      const entry = entries[index];
      if (entry.kind !== 'subject_part') continue;
      const part = entry.props.part as TimelinePart;
      if (!partState(part).committed) continue;
      newest = { key: entry.node_id, label: `第 ${part.step} 轮已提交内容` };
    }
    if (!newest || newest.key === this.announcedEntry) return;
    this.announcedEntry = newest.key;
    this.setState((previous) => (
      previous.announcement === newest.label ? previous : { announcement: newest.label }
    ));
  }

  private onResize = () => {
    const snapshot = this.lastAnchor ?? this.captureAnchor();
    if (this.resizeFrame) cancelAnimationFrame(this.resizeFrame);
    this.resizeFrame = requestAnimationFrame(() => {
      this.resizeFrame = 0;
      if (this.props.node.props.reading === 'following_latest') this.scrollToEnd();
      else if (snapshot) this.restoreAnchor(snapshot);
    });
  };

  private findScrollElement(): HTMLElement | null {
    const viewport = this.capture.current?.querySelector<HTMLElement>('.timeline-viewport');
    if (!viewport) return null;
    const candidates = [viewport, ...Array.from(viewport.querySelectorAll<HTMLElement>('*'))];
    return candidates.find((element) => {
      const overflow = getComputedStyle(element).overflowY;
      return overflow === 'auto' || overflow === 'scroll';
    }) ?? viewport;
  }

  private captureAnchor(): AnchorSnapshot | null {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return null;
    this.scrollElement = scroller;
    const container = scroller.getBoundingClientRect();
    const entries = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'));
    const index = entries.findIndex((entry) => entry.getBoundingClientRect().bottom > container.top + 8);
    if (index < 0) return { scrollTop: scroller.scrollTop };
    const anchor = entries[index];
    return {
      key: anchor.dataset.entryKey,
      offset: anchor.getBoundingClientRect().top - container.top,
      scrollTop: scroller.scrollTop,
      index,
      previousKey: index > 0 ? entries[index - 1].dataset.entryKey : undefined,
    };
  }

  private restoreAnchor(snapshot: AnchorSnapshot) {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    if (snapshot.key) {
      const anchor = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'))
        .find((entry) => entry.dataset.entryKey === snapshot.key);
      if (anchor && typeof snapshot.offset === 'number') {
        const nextOffset = anchor.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
        scroller.scrollTop += nextOffset - snapshot.offset;
        return;
      }
    }
    scroller.scrollTop = snapshot.scrollTop;
  }

  /// Restore the saved place against the entries rendered now. An entry that
  /// no longer exists falls back to its saved predecessor; only then does the
  /// absolute offset stand in, so history above the place never silently
  /// scrolls the reader.
  private restoreSavedAnchor(anchor: ReadingAnchor | undefined) {
    if (!anchor) return;
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    this.scrollElement = scroller;
    const entries = Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'))
      .map((entry) => ({ element: entry, entry_key: entry.dataset.entryKey ?? '' }))
      .filter((entry) => entry.entry_key);
    const resolved = resolveAnchor(anchor, entries);
    if (resolved) {
      const target = entries[resolved.index].element;
      const nextOffset = target.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
      scroller.scrollTop += nextOffset - anchor.offset;
      return;
    }
    scroller.scrollTop = anchor.scroll_top;
  }

  private emitAnchor() {
    if (this.anchorFrame) return;
    this.anchorFrame = requestAnimationFrame(() => {
      this.anchorFrame = 0;
      const snapshot = this.captureAnchor();
      const anchor: ReadingAnchor | undefined = snapshot?.key
        ? {
          entry_key: snapshot.key,
          entry_index: snapshot.index ?? 0,
          ...(snapshot.previousKey ? { prev_entry_key: snapshot.previousKey } : {}),
          offset: snapshot.offset ?? 0,
          scroll_top: snapshot.scrollTop,
        }
        : undefined;
      this.props.emit('timeline', 'timeline.reading', {
        state: 'reading_history',
        at_end: false,
        ...(anchor ? { anchor } : {}),
      });
    });
  }

  private rememberAnchor() {
    this.lastAnchor = this.captureAnchor();
  }

  private entryElements(): HTMLElement[] {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return [];
    return Array.from(scroller.querySelectorAll<HTMLElement>('[data-entry-key]'));
  }

  /// A selection is recorded only inside one entry and only from text-node
  /// endpoints; an element-level boundary has no content offset the view can
  /// replay honestly, so nothing is stored for it.
  private onSelectionChange = () => {
    clearTimeout(this.selectionTimer);
    this.selectionTimer = window.setTimeout(() => {
      this.selectionTimer = undefined;
      const selection = window.getSelection();
      if (!selection || selection.isCollapsed || selection.rangeCount === 0) {
        this.props.emit('timeline', 'timeline.selection', { clear: true });
        return;
      }
      const range = selection.getRangeAt(0);
      if (range.startContainer.nodeType !== Node.TEXT_NODE
        || range.endContainer.nodeType !== Node.TEXT_NODE) return;
      const entries = this.entryElements();
      const startEntry = entries.find((entry) => entry.contains(range.startContainer));
      const endEntry = entries.find((entry) => entry.contains(range.endContainer));
      if (!startEntry || startEntry !== endEntry) return;
      // Selecting committed text is interaction, not "follow the tail": new
      // content must not scroll the reader away from the span they are
      // holding (docs/specs/web-interface.md#spec-21-11).
      this.props.emit('timeline', 'timeline.reading', { state: 'selecting_or_interacting' });
      const entryKey = startEntry.dataset.entryKey;
      if (!entryKey) return;
      const root = textRoot(startEntry);
      const nodes = textNodes(root);
      const start = textOffset(nodes, range.startContainer, range.startOffset);
      const end = textOffset(nodes, range.endContainer, range.endOffset);
      if (start === null || end === null || end <= start) return;
      this.props.emit('timeline', 'timeline.selection', {
        entry_key: entryKey,
        text: root.textContent ?? '',
        start,
        end,
      });
    }, 160);
  };

  /// Replay a stored selection only while the same characters still start the
  /// entry, and never over a live selection the reader is making right now.
  private restoreSavedSelection() {
    // Follow mode means the tail is the place: restoring a highlight there
    // would drag the viewport away from the newest committed text.
    if (this.props.node.props.reading === 'following_latest') return;
    const saved = this.props.node.props.selection as ReadingSelection | undefined;
    if (!saved || saved === this.restoredSelection) return;
    const selection = window.getSelection();
    if (!selection || !selection.isCollapsed) return;
    const entry = this.entryElements().find((row) => row.dataset.entryKey === saved.entry_key);
    if (!entry) return;
    const root = textRoot(entry);
    const text = root.textContent ?? '';
    if (!selectionApplies(saved, text)) return;
    const nodes = textNodes(root);
    const start = pointAt(nodes, saved.start);
    const end = pointAt(nodes, saved.end);
    if (!start || !end) return;
    const range = document.createRange();
    range.setStart(start.node, start.offset);
    range.setEnd(end.node, end.offset);
    selection.removeAllRanges();
    selection.addRange(range);
    this.restoredSelection = saved;
  }

  private scrollToEnd() {
    const scroller = this.scrollElement ?? this.findScrollElement();
    if (!scroller) return;
    this.scrollElement = scroller;
    scroller.scrollTop = scroller.scrollHeight;
  }

  private onScroll = (event: React.UIEvent<HTMLDivElement>) => {
    const target = event.target as HTMLElement;
    if (!this.scrollElement) this.scrollElement = this.findScrollElement();
    if (target !== this.scrollElement) return;
    this.rememberAnchor();
    const atEnd = target.scrollHeight - target.scrollTop - target.clientHeight <= 24;
    if (atEnd) {
      this.props.emit('timeline', 'timeline.reading', {
        state: 'following_latest',
        at_end: true,
      });
      return;
    }
    // Reading upward records a real place: the entry at the viewport top plus
    // its offset, so backfilled history above it does not move the reader.
    this.emitAnchor();
  };

  render() {
    const { node, projection, emit } = this.props;
    const entries = node.props.entries as UiNode[];
    const name = projection.identity?.display_name || undefined;
    return (
      <section className="timeline" hidden={node.props.hidden} aria-label="主体时间线">
        {/* One bounded announcement per committed round; the answer itself is
            not read again. */}
        <span className="sr-only" aria-live="polite">{this.state.announcement}</span>
        <div
          className="timeline-capture"
          ref={this.capture}
          data-reading={node.props.reading}
          data-wrap={node.props.wrap === true ? 'on' : 'off'}
          onScrollCapture={this.onScroll}
          onWheel={(event) => {
            if (event.deltaY < 0) this.emitAnchor();
          }}
          onPointerDown={(event) => {
            if ((event.target as Element).closest('pre,button,details,input,textarea,select,[role="button"]')) {
              emit('timeline', 'timeline.reading', { state: 'selecting_or_interacting' });
            }
          }}
        >
          <ScrollRoot className="timeline-root">
            <ScrollViewport className="timeline-viewport">
              <div className="timeline-column">
                <SubmissionHistory node={node} emit={emit} />
                {entries.length === 0
                  ? <EmptyTimeline connection={projection.connection} demo={projection.source === 'demo'} name={name} />
                  : <TimelineEntries entries={entries} emit={emit} name={name} onOpenWork={this.props.onOpenWork} />}
              </div>
            </ScrollViewport>
            <ScrollScrollbar orientation="vertical" className="timeline-scrollbar">
              <ScrollThumb className="timeline-thumb" />
            </ScrollScrollbar>
          </ScrollRoot>
          {node.props.reading !== 'following_latest' && (
            <div className="jump-latest">
              <Button variant="secondary" size="sm" onClick={() => emit('timeline', 'timeline.latest')}>
                <Icon name="down" size={15} />
                {node.props.new_count > 0 ? `${node.props.new_count} 条新内容` : '回到最新'}
              </Button>
            </div>
          )}
        </div>
      </section>
    );
  }
}

function EmptyTimeline({ connection, demo }: { connection: ConnectionState; demo: boolean; name?: string }) {
  return (
    <div className="empty" data-testid="empty-timeline">
      <Icon name="timeline" size={22} />
      <p>
        {demo
          ? '模拟时间线为空。'
          : connection === 'online'
            ? '还没有已提交的内容。'
            : '没有连上服务，草稿会保留。'}
      </p>
    </div>
  );
}

function entryTime(entry: UiNode): string | null {
  if (entry.kind === 'subject_part') return entry.props.part.observed_at;
  if (entry.kind === 'user_submission') return entry.props.submission.created_at;
  return null;
}

function TimelineEntries({ entries, emit, name, onOpenWork }: {
  entries: UiNode[]; emit: Emit; name?: string; onOpenWork: (artifactId: string) => void;
}) {
  let lastDay = '';
  let previous: { kind: string; at: number } | undefined;
  return <div className="entries">{entries.map((entry) => {
    // A document (or its failure) rides the day of the committed step that
    // carried it; it never opens a "时间未知" group of its own.
    if (entry.kind === 'semantic_document' || entry.kind === 'document_error' || entry.kind === 'work') {
      previous = undefined;
      return <EntryView node={entry} emit={emit} key={entry.node_id} name={name} continued={false} onOpenWork={onOpenWork} />;
    }
    const value = entryTime(entry);
    const day = dayLabel(value);
    const separator = day !== lastDay
      ? <div className="day" key={`day-${entry.node_id}`}><Separator className="day-rule" /><span>{day}</span><Separator className="day-rule" /></div>
      : null;
    if (separator) previous = undefined;
    lastDay = day;
    // Consecutive subject rounds within a few minutes read as one voice: the
    // seal and name are shown once, each round keeps its own meta line.
    const at = value ? Date.parse(value) : Number.NaN;
    const continued = previous !== undefined
      && previous.kind === entry.kind
      && Number.isFinite(at)
      && at - previous.at < 6 * 60_000;
    previous = { kind: entry.kind, at };
    return <React.Fragment key={entry.node_id}>
      {separator}
      <EntryView node={entry} emit={emit} name={name} continued={continued} onOpenWork={onOpenWork} />
    </React.Fragment>;
  })}</div>;
}

function EntryView({ node, emit, name, continued, onOpenWork }: {
  node: UiNode; emit: Emit; name?: string; continued: boolean; onOpenWork: (artifactId: string) => void;
}) {
  switch (node.kind) {
    case 'subject_part': return <SubjectPart node={node} emit={emit} name={name} continued={continued} />;
    case 'semantic_document': return <SemanticDocumentView node={node} emit={emit} />;
    case 'document_error': return <DocumentErrorEntry node={node} />;
    case 'work': return <WorkEntry node={node} emit={emit} onOpenWork={onOpenWork} />;
    default: return <UserSubmission node={node} emit={emit} continued={continued} />;
  }
}

function SubjectPart({ node, emit, name, continued }: { node: UiNode; emit: Emit; name?: string; continued: boolean }) {
  const part = node.props.part as TimelinePart;
  const state = partState(part);
  const live = state.streaming && !part.terminal;
  // Only committed text carries a step number. A durable streamed prefix is
  // real ledger content, but labelling it "第 N 轮" would claim an S1 step the
  // part does not have yet (docs/specs/web-interface.md#spec-21-11).
  const stepLabel = state.committed ? `第 ${part.step} 轮` : part.terminal ? '流式 · 已结束' : '正在输出';
  const announcement = state.committed
    ? `第 ${part.step} 轮已提交内容`
    : '主体正在输出、尚未提交的持久流式内容';
  return (
    <article
      className={`entry subject${continued ? ' is-continued' : ''}`}
      data-entry-key={node.node_id}
      data-message-id={part.message_id}
      data-part-id={part.part_id}
      data-part-revision={part.part_revision}
      data-next-chunk-seq={part.next_chunk_seq}
      data-commit-ref={part.commit_ref}
      data-committed={state.committed ? 'true' : 'false'}
      data-streaming={state.streaming ? 'true' : 'false'}
      aria-label={announcement}
      aria-busy={live ? true : undefined}
    >
      <div className="entry-main">
        <header className="entry-meta">
          <span className={`entry-step${state.committed ? '' : ' is-live'}`}>
            {live && <span className="live-dot" aria-hidden="true" />}
            {stepLabel}
          </span>
          <time dateTime={part.observed_at} title="界面接收时间，不是持久 commit 墙钟时间">{formatTime(part.observed_at)}</time>
          {!state.committed && <span className="tag tag-quiet">未提交</span>}
          {part.gap && <span className="tag tag-warn">序号空洞 · 等待补读</span>}
        </header>
        <div className="entry-body">
          <PartErrorBoundary text={part.content}><Markdown text={part.content} /></PartErrorBoundary>
        </div>
        {state.committed && <DelegationBranches stepId={part.step_id} />}
        <footer className="entry-tools">
          <Button variant="ghost" size="sm" label="复制这一轮" onClick={() => emit(node.node_id, node.action_refs[0])}>
            <Icon name="copy" size={15} /><span>复制</span>
          </Button>
          <CollapsibleRoot className="identity">
            <CollapsibleTrigger className="identity-trigger">
              <Icon name="info" size={15} /><span>来源</span>
            </CollapsibleTrigger>
            <CollapsibleContent className="identity-body"><dl>
              <div><dt>message</dt><dd>{part.message_id}</dd></div>
              <div><dt>part</dt><dd>{part.part_id}</dd></div>
              <div><dt>revision</dt><dd>{part.part_revision}</dd></div>
              <div><dt>commit</dt><dd>{part.commit_ref}</dd></div>
              <div><dt>request / attempt</dt><dd>{part.request_id ?? '未知'} / {part.attempt_id ?? '未知'}</dd></div>
              <div><dt>状态</dt><dd>{state.committed ? '已提交' : '未提交 · 持久流式'}</dd></div>
            </dl></CollapsibleContent>
          </CollapsibleRoot>
        </footer>
      </div>
    </article>
  );
}

class PartErrorBoundary extends React.Component<{ text: string; children: React.ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) {
      return <div className="part-fallback" role="note">
        <p>这一段没能按格式呈现，下面是原文；其余内容不受影响。</p>
        <pre>{this.props.text}</pre>
        <button type="button" onClick={() => this.setState({ failed: false })}>重新呈现</button>
      </div>;
    }
    return this.props.children;
  }
}

// Durable admission stages are distinct facts (docs/specs/web-interface.md#spec-41-18):
// each label names one stage; none is drawn as a progress bar.
const RECEIPT: Record<string, { short: string; long: string; tone: string }> = {
  sending: { short: '发送中', long: '正在写入传输', tone: 'pending' },
  sent: { short: '已发出', long: '已写入传输 · 等待服务端回执', tone: 'pending' },
  accepted: { short: '已接纳', long: '已持久接纳 · 等待选入/处置回执', tone: 'accepted' },
  selected: { short: '已选入', long: '已选入封口输入计划 · 等待处置回执', tone: 'accepted' },
  committed: { short: '已处置', long: '主体已提交处置', tone: 'done' },
  conflict: { short: 'ID 冲突', long: '提交 ID 冲突 · 原输入未改写', tone: 'error' },
  rejected: { short: '未接收', long: '服务端未接收', tone: 'error' },
  unknown: { short: '回执未知', long: '传输回执未知 · 可用同一 ID 查询或重试', tone: 'warn' },
};

function placementText(submission: Submission): string | null {
  const placement = submission.placement;
  if (!placement) return null;
  switch (placement.actual) {
    case 'next_boundary': return '下一边界处置';
    case 'live_steer': return '进行中请求内处置';
    case 'cancel_reseal': return '取消重封';
    default: return '处置位置待定';
  }
}

function UserSubmission({ node, emit, continued }: { node: UiNode; emit: Emit; continued: boolean }) {
  const submission = node.props.submission as Submission;
  const receipt = RECEIPT[submission.receipt] ?? RECEIPT.unknown;
  const withdrawal = submission.withdrawal;
  const withdrawAction = node.props.withdraw_action as string | undefined;
  const retryAction = node.props.retry_action as string | undefined;
  const placement = placementText(submission);
  const steer = submission.payload.mode === 'steer';
  const detail = [receipt.long, submission.detail, placement].filter(Boolean).join(' · ');
  return (
    <article
      className={`entry user${continued ? ' is-continued' : ''}${withdrawal ? ' is-withdrawn' : ''}`}
      data-entry-key={node.node_id}
      data-submission-id={submission.payload.submission_id}
    >
      <div className="entry-main">
        <div className="bubble">
          {steer && <span className="bubble-mode"><Icon name="steer" size={13} />引导</span>}
          <div className="entry-body">{submission.payload.content}</div>
        </div>
        <footer className={`receipt tone-${receipt.tone}`} data-receipt={submission.receipt} title={detail}>
          <span className="receipt-dot" aria-hidden="true" />
          <span className="receipt-label">{receipt.short}</span>
          <span className="sr-only">：{detail}</span>
          {submission.detail && <span className="receipt-detail">{submission.detail}</span>}
          {placement && <span className="receipt-detail">{placement}</span>}
          {withdrawal && (
            <span className="receipt-detail is-withdrawal">
              {withdrawal.result === 'withdrawal_queued' ? '撤回已排队' : '呈现前已撤回'}
            </span>
          )}
          <time dateTime={submission.created_at}>{formatTime(submission.created_at)}</time>
          {(submission.receipt === 'rejected' || submission.receipt === 'unknown') && typeof retryAction === 'string' && (
            <button type="button" className="text-action" onClick={() => emit(node.node_id, retryAction)}>
              <Icon name="retry" size={14} />以同一 ID 重试
            </button>
          )}
          {!withdrawal && typeof withdrawAction === 'string' && (
            <button type="button" className="text-action" onClick={() => emit(node.node_id, withdrawAction)}>
              <Icon name="undo" size={14} />撤回
            </button>
          )}
        </footer>
      </div>
    </article>
  );
}

/// One work of the subject, in the timeline (docs/specs/configuration-playground.md#spec-57-6).
///
/// It names the work's own revision: what the reader does next is bound to
/// the revision they saw. Opening the work is the reader's own action.
function WorkEntry({ node, emit, onOpenWork }: { node: UiNode; emit: Emit; onOpenWork: (artifactId: string) => void }) {
  const work = node.props.work as WorkView;
  // Page works offer both the live page and their revision-bound management
  // record; all work actions still use the projected action identity.
  const page = isPage(work.mime);
  return (
    <article className="entry attachment" data-entry-key={node.node_id} data-artifact-id={work.artifact_id}>
      <div className="entry-main">
        <div className="work-card">
          <span className="work-icon"><Icon name="file" /></span>
          <span className="work-text">
            <strong>{String(work.title ?? work.artifact_id)}</strong>
            <span className="work-meta">
              作品 · r{String(work.revision ?? '—')} · {String(work.lifecycle ?? 'draft')} · {String(work.mime ?? '—')}
            </span>
          </span>
          <Button
            variant="outline"
            size="sm"
            onClick={() => page ? onOpenWork(work.artifact_id) : emit(node.node_id, node.action_refs[0])}
          >
            <Icon name="open" size={15} />打开
          </Button>
          {page && <Button variant="ghost" size="sm" onClick={() => emit(node.node_id, node.action_refs[0])}>
            详情与操作
          </Button>}
        </div>
      </div>
    </article>
  );
}

// The rich layer is gone; the committed text beside it stays the record and
// the reason is readable instead of only flashing as a notice (docs/research/product-positioning.md#spec-3).
function DocumentErrorEntry({ node }: { node: UiNode }) {
  const error = node.props.error as DocumentErrorView;
  return (
    <article
      className="entry attachment"
      data-entry-key={node.node_id}
      data-source-step={error.source_step_id}
      role="note"
    >
      <div className="entry-main">
        <div className="doc-error">
          <Icon name="warning" />
          <div>
            <p>这一轮里的界面块没能呈现，已退回原文显示。</p>
            <p className="doc-error-reason">{error.reason}</p>
            {error.source_commit && <code>commit {error.source_commit}</code>}
          </div>
        </div>
      </div>
    </article>
  );
}

// A degraded semantic document: the mode decides how much structure renders,
// but the numbered options are always the same action identities (docs/specs/web-interface.md#spec-21-8).
// Execution authority comes from the subject's disposition of the admitted
// input; this view only submits that input.
const ACTION_STAGE_LABELS: Record<ActionReceiptView['stage'], string> = {
  sending: '提交中',
  accepted: '已接纳 · 等待主体处置',
  selected: '已选入封口输入计划',
  committed: '主体已处置',
  rejected: '未接纳',
  unavailable: '回执暂不可用',
  conflict: '动作键冲突',
};

function SemanticDocumentView({ node, emit }: { node: UiNode; emit: Emit }) {
  const document = node.props.document as DegradedDocument;
  const receipts = (node.props.receipts ?? []) as ActionReceiptView[];
  const latest = (actionId: string): ActionReceiptView | undefined => receipts
    .filter((row) => row.action_id === actionId)
    .at(-1);
  // An admitted decision awaiting disposition must not be admitted twice by a
  // second click; the option stays visible with its durable stage shown.
  const admitted = (row: ActionReceiptView | undefined): boolean =>
    row !== undefined && ['sending', 'accepted', 'selected'].includes(row.stage);
  return (
    <article className="entry attachment" data-entry-key={node.node_id} data-document-id={document.document_id}>
      <div className="entry-main">
        <div className={`doc is-${document.mode}`}>
          <header className="doc-head">
            <span className="doc-kind"><Icon name="spark" size={15} />界面</span>
            <span className="doc-mode" data-mode={document.mode}>
              {document.mode === 'rich' ? '完整' : document.mode === 'card' ? '卡片' : '文本'}
            </span>
          </header>
          <p className="doc-summary">{document.summary}</p>
          {document.mode === 'rich' && Array.isArray(document.nodes) && document.nodes.length > 0 && (
            <RichDocument
              nodes={document.nodes as DocumentNodeView[]}
              options={document.options}
              actionRefs={node.action_refs ?? []}
              onAction={(actionRef) => emit(node.node_id, actionRef)}
              renderMarkdown={(markdown) => <Markdown text={markdown} />}
            />
          )}
          {document.options.length > 0 && (
            <ol className="doc-options">
              {document.options.map((option, index) => {
                const receipt = latest(option.action_id);
                const pending = admitted(receipt);
                return (
                  <li key={`${option.action_id}:${option.nonce}`} data-action-id={option.action_id}>
                    <button
                      type="button"
                      className="doc-option"
                      disabled={pending || !node.action_refs[index]}
                      onClick={() => emit(node.node_id, node.action_refs[index])}
                    >
                      <span className="doc-option-index">{index + 1}</span>
                      <span className="doc-option-label">{option.label}</span>
                      {receipt && (
                        <span className={`doc-option-receipt is-${receipt.stage}`} data-stage={receipt.stage}>
                          {ACTION_STAGE_LABELS[receipt.stage]}{receipt.detail ? ` · ${receipt.detail}` : ''}
                        </span>
                      )}
                    </button>
                  </li>
                );
              })}
            </ol>
          )}
        </div>
      </div>
    </article>
  );
}

function Composer({ node, emit, variant = 'stream' }: { node: UiNode; emit: Emit; variant?: 'stream' | 'im' }) {
  const draftAction = node.action_refs[0];
  const appendAction = node.action_refs[1];
  const steerAction = node.props.steer_action as string | undefined;
  const append = node.props.append_action as string;
  const canSend = Boolean(node.props.append_enabled) && appendAction === append;
  const canSteer = Boolean(node.props.steer_enabled) && steerAction === node.action_refs[2];
  // The mode is the reader's choice of which admitted action Enter submits;
  // steer is offered only while the server provides it.
  const [mode, setMode] = React.useState<'append' | 'steer'>('append');
  const steerOffered = node.props.steer_available === true && steerAction === node.action_refs[2];
  const steering = mode === 'steer' && steerOffered;
  const submit = () => {
    if (steering && steerAction) emit('composer', steerAction);
    else if (canSend) emit('composer', appendAction);
  };
  const demo = node.props.demo === true;
  const reason = demo
    ? '本地模拟投影，不发送输入'
    : node.props.connection !== 'online'
      ? '离线中 · 草稿保留在此浏览器'
      : !node.props.subject_bound
        ? '等待真实主体绑定'
        : node.props.append_offered !== true
          ? '服务端当前未开放追加输入'
          : null;
  return (
    <footer className={`composer composer-${variant}`} hidden={node.props.hidden}>
      <div className={`composer-card${steering ? ' is-steer' : ''}`}>
        <div
          className="composer-editor"
          id="composer-editor"
          onKeyDownCapture={(event) => {
            if (event.nativeEvent.isComposing) return;
            if (event.key === 'Enter' && !event.shiftKey) {
              event.preventDefault();
              submit();
            } else if (event.key === 'Escape') {
              event.stopPropagation();
              if (node.props.draft) emit('composer', draftAction, { value: '' });
              else (event.target as HTMLElement).blur();
            }
          }}
        >
          <ProtoTextarea
            ariaLabel={demo ? '模拟投影输入区，不发送' : steering ? '引导同一主体' : '向同一主体追加输入'}
            value={node.props.draft}
            rows={2}
            readOnly={demo}
            placeholder={demo ? '模拟投影不发送输入' : steering ? '引导它此刻的注意力…' : variant === 'im' ? '信息' : '说点什么…'}
            className="composer-input"
            onValueChange={(detail) => emit('composer', draftAction, { value: detail.value })}
          />
        </div>
        <div className="composer-bar">
          {steerOffered ? (
            <ToggleGroupRoot
              className="mode-switch"
              a11yLabel="输入方式"
              value={[steering ? 'steer' : 'append']}
              onValueChange={(detail: { value: readonly string[] }) => {
                const next = detail.value[0];
                if (next === 'append' || next === 'steer') setMode(next);
              }}
            >
              <ToggleGroupItem className="mode-item" value="append">追加</ToggleGroupItem>
              <ToggleGroupItem className="mode-item" value="steer">引导</ToggleGroupItem>
            </ToggleGroupRoot>
          ) : null}
          <span className="composer-hint">
            {reason ?? (steering ? '引导在下一边界处置 · Enter 发送' : 'Enter 发送 · Shift + Enter 换行')}
          </span>
          <Button
            variant="default"
            size="icon"
            className="send"
            label={steering ? '引导' : '追加'}
            disabled={steering ? !canSteer : !canSend}
            onClick={submit}
          ><Icon name={steering ? 'steer' : 'send'} /></Button>
        </div>
      </div>
    </footer>
  );
}

/// The rendered text of one entry, excluding its chrome (meta line, receipt
/// footer): a selection replay must address the content the reader saw.
function textRoot(entry: HTMLElement): HTMLElement {
  return entry.querySelector<HTMLElement>('.entry-body') ?? entry;
}

function textNodes(root: HTMLElement): Text[] {
  const walker = document.createTreeWalker(root, NodeFilter.SHOW_TEXT);
  const nodes: Text[] = [];
  for (let node = walker.nextNode(); node; node = walker.nextNode()) {
    if (node.nodeType === Node.TEXT_NODE) nodes.push(node as Text);
  }
  return nodes;
}

function textOffset(nodes: Text[], container: Node, offset: number): number | null {
  let total = 0;
  for (const node of nodes) {
    if (node === container) return total + offset;
    total += node.data.length;
  }
  return null;
}

function pointAt(nodes: Text[], offset: number): { node: Text; offset: number } | null {
  let remaining = offset;
  for (const node of nodes) {
    const length = node.data.length;
    if (remaining <= length) return { node, offset: remaining };
    remaining -= length;
  }
  const last = nodes[nodes.length - 1];
  return last ? { node: last, offset: last.data.length } : null;
}
