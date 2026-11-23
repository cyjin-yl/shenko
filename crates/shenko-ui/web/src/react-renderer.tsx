import { text as t, resolveMessage } from './messages';
import * as React from 'react';
import { createRoot, type Root } from 'react-dom/client';
import { intentFor, type Dispatch, type TaskSummary, type UiNode, type UiProjection, type ViewName, type WorkView } from './projection';
import { Icon } from './icons';
import {
  Button, DrawerContent, DrawerHeader, DrawerMask, DrawerRoot, DrawerTitle,
  ToggleGroupItem, ToggleGroupRoot, Toggle, TransportBadge, connectionAnnouncement, type Emit,
} from './ui-kit';
import { ConfigSurface, ContextPanel, MemorySurface, PlaygroundSurface, TasksSurface } from './surfaces';
import { ImView } from './im';
import { FilesView, liveFiles } from './files-view';
import { collectThreads, isAttentionTask, subjectParticipant, type DemoThreadData, type ThreadMessage } from './threads';
import { lensSnapshot, subscribeLenses, syncWorkLenses, type LensContribution } from './slots';
import { WorkFrame } from './work-frame';
import { InspectContext } from './delegation-branches';
import { TimelineSurface } from './timeline-surface';
import { SemanticDocumentView } from './timeline-entries';
import { Composer } from './composer';
import { SubmissionHistory } from './submission-history';
import { InspectorLayout } from './inspector-layout';
import { useConversations } from './conversation-data';

export interface ProjectionRenderer {
  render(projection: UiProjection, dispatch: Dispatch): void;
  patch(projection: UiProjection): void;
  dispose(): void;
}

export function createReactProjectionRenderer(host: HTMLElement, demoData?: DemoThreadData): ProjectionRenderer {
  const root: Root = createRoot(host);
  let disposed = false;
  let dispatch: Dispatch = () => undefined;
  const patch = (projection: UiProjection) => {
    if (disposed) return;
    root.render(<ProjectionView projection={projection} dispatch={dispatch} demoData={demoData} />);
  };
  return {
    render(projection, nextDispatch) {
      dispatch = nextDispatch;
      patch(projection);
    },
    patch,
    dispose() {
      if (disposed) return;
      disposed = true;
      root.unmount();
    },
  };
}

/** The stream's lens: whichever contribution is open beside it. */
type OpenLens = { kind: 'view'; view: ViewName } | { kind: 'work'; artifact_id: string } | null;

function ProjectionView({ projection, dispatch, demoData }: { projection: UiProjection; dispatch: Dispatch; demoData?: DemoThreadData }) {
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
  const [tab, setTabState] = React.useState<'im' | 'inspect' | 'files'>(() => {
    const query = new URLSearchParams(location.search).get('tab');
    if (query === 'inspect' || query === 'im' || query === 'files') return query;
    try {
      const saved = localStorage.getItem('shenko.tab.v1');
      return saved === 'inspect' || saved === 'files' ? saved : 'im';
    } catch { return 'im'; }
  });
  const setTab = (next: 'im' | 'inspect' | 'files') => {
    setTabState(next);
    try { localStorage.setItem('shenko.tab.v1', next); } catch { /* page-only preference */ }
  };
  const [threadId, setThreadId] = React.useState<string>(() => new URLSearchParams(location.search).get('thread') ?? 'direct');
  const conversations = useConversations(projection.source === 'live', timeline.props.step as number | null);
  const threads = React.useMemo(
    () => collectThreads({ projection, tasks, conversations: demoData?.conversations ?? conversations.rows, demo: demoData }),
    [projection, tasks, conversations.rows, demoData],
  );
  const inspect = React.useMemo(() => ({
    tasks,
    openThread: (id: string) => { setThreadId(id); setTab('im'); },
    subject: subjectParticipant(projection),
    demo: demoData,
  }), [tasks, projection, demoData]);
  // Phones get one tab bar of real destinations; every lens moves into the
  // 更多 sheet. Desktop keeps the top tabs and the lens bar.
  const [moreOpen, setMoreOpen] = React.useState(false);
  const openTab = (next: 'im' | 'inspect' | 'files') => {
    if (open !== null) openLens(undefined);
    setTab(next);
  };
  const openWork = open?.kind === 'work'
    ? (works ?? knownWorks.current).find((work) => work.artifact_id === open.artifact_id)
    : undefined;

  return (
    <div className="shell" data-source={projection.source} data-lens={openId ?? 'none'}>
      <a className="skip-link" href="#composer-editor" onClick={(event) => {
        event.preventDefault();
        document.querySelector<HTMLTextAreaElement>('#composer-editor textarea')?.focus();
      }}>{t('shell.skipComposer')}</a>

      <header className="topbar">
        <button
          type="button"
          className="presence"
          onClick={() => emit('context', 'panel.toggle')}
          aria-label={t('shell.subjectDetails', { name: name ?? t('subject.unnamed') })}
        >
          <span className="presence-name">
            {name ?? <span className="is-unnamed">{t('subject.unnamed')}</span>}
            {demo && <span className="tag">{t('shell.demo')}</span>}
          </span>
          <Attention step={timeline.props.step} status={timeline.props.subject_status} tasks={tasks} />
        </button>
        <nav className="tabs" aria-label={t('shell.views')}>
          <ToggleGroupRoot
            className="tab-switch"
            a11yLabel={t('shell.views')}
            value={[tab]}
            onValueChange={(detail: { value: readonly string[] }) => {
              const next = detail.value.find((value) => value !== tab);
              if (next === 'im' || next === 'inspect' || next === 'files') setTab(next);
            }}
          >
            <ToggleGroupItem value="im" className="tab-item"><Icon name="chat" size={16} />{t('shell.chat')}</ToggleGroupItem>
            <ToggleGroupItem value="inspect" className="tab-item"><Icon name="timeline" size={16} />Inspect</ToggleGroupItem>
            <ToggleGroupItem value="files" className="tab-item"><Icon name="files" size={16} />{t('files.title')}</ToggleGroupItem>
          </ToggleGroupRoot>
        </nav>
        <nav className="lensbar" aria-label={t('shell.lenses')}>
          <ToggleGroupRoot
            className="lens-switch"
            a11yLabel={t('shell.openLens')}
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
            label={t('shell.wrap')}
            onChange={() => emit('timeline', 'timeline.code_wrap')}
          ><Icon name="wrap" /></Toggle>
          <Button
            variant="ghost"
            size="icon"
            label={theme === 'light' ? t('shell.dark') : t('shell.light')}
            onClick={() => emit('navigation', 'theme.toggle')}
          ><Icon name={theme === 'light' ? 'moon' : 'sun'} /></Button>
          <Button variant="ghost" size="icon" label={t('subject.details')} onClick={() => emit('context', 'panel.toggle')}>
            <Icon name="panel" />
          </Button>
        </div>
      </header>

      <InspectorLayout
        openId={openId}
        title={String(lenses.find((lens) => lens.id === openId)?.title ?? openWork?.title ?? '')}
        subtitle={open?.kind === 'work' ? t('shell.workLens') : undefined}
        onClose={() => openLens(undefined)}
        inspector={open?.kind === 'work'
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
      >
        <main className="stream" id="main" data-tab={tab}>
          {tab === 'files'
            ? <FilesView source={demoData?.files ?? liveFiles} />
            : tab === 'im'
            ? <ImView
              key={JSON.stringify([projection.surface_id, projection.source, projection.subject_id])}
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
                key={JSON.stringify([projection.surface_id, projection.source, projection.subject_id])}
                node={timeline}
                projection={projection}
                emit={emit}
                onOpenWork={(artifactId) => openLens({ id: `work:${artifactId}`, title: '', icon: 'file', order: 0, source: 'work', artifact_id: artifactId })}
              />
              <Composer node={composer} emit={emit} />
            </InspectContext.Provider>}
          {/* Results of the reader's own acts (copied, rejected stale action…)
              are a quiet status line, not a floating toast. */}
          <div className="statusline" role="status" aria-live="polite">{[resolveMessage(notice.props.text, projection.locale), conversations.error].filter(Boolean).join(' · ')}</div>
        </main>
      </InspectorLayout>

      <nav className="tabbar" aria-label={t('shell.views')}>
        {([
          ['im', 'chat', t('shell.chat')],
          ['inspect', 'timeline', 'Inspect'],
          ['files', 'files', t('files.title')],
        ] as const).map(([id, icon, label]) => (
          <button
            key={id}
            type="button"
            className="tabbar-item"
            aria-current={open === null && tab === id ? 'page' : undefined}
            onClick={() => openTab(id)}
          >
            <Icon name={icon} size={22} />
            <span>{label}</span>
          </button>
        ))}
        <button
          type="button"
          className="tabbar-item"
          aria-current={open !== null ? 'page' : undefined}
          aria-haspopup="dialog"
          aria-expanded={moreOpen}
          onClick={() => setMoreOpen(true)}
        >
          <Icon name="more" size={22} />
          <span>{t('shell.more')}</span>
        </button>
      </nav>
      <DrawerRoot
        className="sheet-root"
        open={moreOpen}
        a11yLabel={t('shell.more')}
        onOpenChange={(detail: { open: boolean }) => setMoreOpen(detail.open)}
      >
        <DrawerMask className="sheet-mask" />
        <DrawerContent side="bottom" className="sheet more-sheet">
          <DrawerHeader className="sheet-head">
            <div className="sheet-title">
              <DrawerTitle className="sheet-name">{t('shell.more')}</DrawerTitle>
            </div>
            <Button variant="ghost" size="icon" label={t('shell.closeMore')} onClick={() => setMoreOpen(false)}>
              <Icon name="close" />
            </Button>
          </DrawerHeader>
          <ul className="more-list">
            {lenses.map((lens) => (
              <li key={lens.id}>
                <button
                  type="button"
                  className="more-item"
                  aria-current={openId === lens.id ? 'true' : undefined}
                  onClick={() => { setMoreOpen(false); openLens(lens); }}
                >
                  <Icon name={lens.icon} size={20} />
                  <span className="more-label">{lens.title}</span>
                  <Icon name="chevron" size={16} />
                </button>
              </li>
            ))}
          </ul>
        </DrawerContent>
      </DrawerRoot>

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
  if (active.length === 1) facts.push(t('shell.activeOne', { kind: String(active[0].kind ?? active[0].task_id) }));
  else if (active.length > 1) facts.push(t('shell.activeMany', { count: active.length }));
  facts.push(step ? t('timeline.round', { step: step ?? 0 }) : t('shell.noSteps'));
  if (status && status !== 'unknown') facts.push(status);
  return <span className="attention">{facts.join(' · ')}</span>;
}
