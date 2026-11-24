import * as React from 'react';
import { text as t } from './messages';
import type { PlaygroundView, TaskSummary } from './projection';
import { Icon } from './icons';
import { isPage } from './slots';
import { isAttentionTask, isDelegation, shortId, taskStateLabel } from './threads';
import {
  Button, ScopeTabs, TreeRoot, TreeItem, TreeGroup, type InspectorTreeHandle, formatCompactAge, formatStamp,
} from './ui-kit';
import {
  artifactEntries, taskEntries, presentHierarchy,
  type HierarchyEntry, type PresentedHierarchy,
} from './inspector-tree-model';

export interface PendingDecisionNotice {
  summary: string;
  onReview: () => void;
}

export function PendingDecisionBanner({ decision }: { decision: PendingDecisionNotice }) {
  return (
    <div className="inspector-attention">
      <span className="inspector-attention-dot" aria-hidden="true" />
      <div className="inspector-attention-text">
        <strong>{t('inspector.attentionTitle')}</strong>
        <span>{decision.summary}</span>
      </div>
      <button type="button" className="inspector-attention-cta" onClick={decision.onReview}>
        {t('inspector.attentionOpen')}
      </button>
    </div>
  );
}

const DAY_MS = 86_400_000;
const DORMANT_HORIZON_MS = 7 * DAY_MS;

type TaskScope = 'recent' | 'live' | 'topics' | 'archive';

interface HorizonBucket<T> {
  key: string;
  label: string;
  dormant: boolean;
  items: T[];
}

interface TopicItem {
  id: string;
  title: string;
  subtitle: string;
  teamCount?: number;
  primaryTask: TaskSummary;
  active: boolean;
  state: string;
  createdAt?: string;
  createdMs: number;
  threadId?: string;
  searchText: string;
}

function taskTimestamp(task: TaskSummary): number {
  return task.created_at ? Date.parse(task.created_at) : NaN;
}

function delegationThreadId(task: TaskSummary): string | undefined {
  if (!isDelegation(task)) return undefined;
  return task.team
    ? `team:${task.created_by_step_id ?? ''}|${task.team}`
    : `delegation:${task.task_id}`;
}

/** Detect any structural defect so invalid graphs pass unchanged to public Tree. */
function hasGraphDefect(tasks: readonly TaskSummary[]): boolean {
  const ids = new Set<string>();
  for (const task of tasks) {
    if (!task.task_id || ids.has(task.task_id)) return true;
    ids.add(task.task_id);
  }
  const parentOf = new Map<string, string>();
  for (const task of tasks) {
    if (task.parent_task_id) {
      if (!ids.has(task.parent_task_id)) return true;
      parentOf.set(task.task_id, task.parent_task_id);
    }
  }
  const mark = new Map<string, 1 | 2>();
  for (const id of ids) {
    let cur: string | undefined = id;
    const trail: string[] = [];
    while (cur && !mark.has(cur)) {
      mark.set(cur, 1);
      trail.push(cur);
      cur = parentOf.get(cur);
    }
    if (cur && mark.get(cur) === 1) return true;
    for (const node of trail) mark.set(node, 2);
  }
  return false;
}

/**
 * Partition valid long-lived histories into recent/active tasks (kept in the
 * live lineage tree) and dormant historical tasks (> 7d older than the newest
 * task, never active, never root, never ancestor of a kept task).
 * Any structurally invalid graph skips partitioning so public Tree validates
 * every original record verbatim.
 */
function partitionTasksByHorizon(tasks: readonly TaskSummary[]): {
  recent: TaskSummary[];
  dormant: TaskSummary[];
  newestMs: number | null;
} {
  let newestMs: number | null = null;
  for (const task of tasks) {
    const ts = taskTimestamp(task);
    if (Number.isFinite(ts) && (newestMs === null || ts > newestMs)) newestMs = ts;
  }
  if (newestMs === null || hasGraphDefect(tasks)) {
    return { recent: [...tasks], dormant: [], newestMs };
  }
  const byId = new Map(tasks.map((task) => [task.task_id, task]));
  const kept = new Set<string>();
  for (const task of tasks) {
    const ts = taskTimestamp(task);
    if (
      task.role === 'subject_root'
      || isAttentionTask(task)
      || !Number.isFinite(ts)
      || (newestMs - ts) <= DORMANT_HORIZON_MS
    ) {
      kept.add(task.task_id);
    }
  }
  for (const id of [...kept]) {
    let parentId = byId.get(id)?.parent_task_id;
    while (parentId && !kept.has(parentId)) {
      kept.add(parentId);
      parentId = byId.get(parentId)?.parent_task_id;
    }
  }
  const recent: TaskSummary[] = [];
  const dormant: TaskSummary[] = [];
  for (const task of tasks) {
    if (kept.has(task.task_id)) recent.push(task);
    else dormant.push(task);
  }
  dormant.sort((a, b) => (taskTimestamp(b) || 0) - (taskTimestamp(a) || 0));
  return { recent, dormant, newestMs };
}

function horizonFor(createdMs: number, active: boolean, newestMs: number): { key: string; label: string; dormant: boolean } {
  if (active) return { key: 'live', label: t('tasks.bucket.live'), dormant: false };
  if (!Number.isFinite(createdMs)) return { key: 'today', label: t('tasks.bucket.today'), dormant: false };
  const delta = Math.max(0, newestMs - createdMs);
  if (delta <= DAY_MS) return { key: 'today', label: t('tasks.bucket.today'), dormant: false };
  if (delta <= DORMANT_HORIZON_MS) return { key: 'week', label: t('tasks.bucket.week'), dormant: false };
  if (delta <= 30 * DAY_MS) return { key: 'month', label: t('tasks.bucket.month'), dormant: true };
  const months = Math.max(1, Math.floor(delta / (30 * DAY_MS)));
  if (months < 12) return { key: `mo:${months}`, label: t('tasks.bucket.monthsAgo', { months }), dormant: true };
  return { key: 'older', label: t('tasks.bucket.older'), dormant: true };
}

function groupByHorizon<T>(
  items: readonly T[],
  getTime: (item: T) => number,
  isLive: (item: T) => boolean,
  newestMs: number,
): HorizonBucket<T>[] {
  const buckets = new Map<string, HorizonBucket<T>>();
  for (const item of items) {
    const info = horizonFor(getTime(item), isLive(item), newestMs);
    const existing = buckets.get(info.key);
    if (existing) existing.items.push(item);
    else buckets.set(info.key, { ...info, items: [item] });
  }
  return [...buckets.values()];
}

/**
 * Flatten lifelong delegations into topic/team cards: team members sharing
 * (created_by_step_id, team) merge into a single team topic row rather than
 * repeating the team under a deep dropdown tree.
 */
function buildTopics(tasks: readonly TaskSummary[]): TopicItem[] {
  const teams = new Map<string, TaskSummary[]>();
  const singles: TaskSummary[] = [];
  for (const task of tasks) {
    if (task.role === 'subject_root' || task.role === 'tool') continue;
    if (isDelegation(task) && task.team) {
      const key = `${task.created_by_step_id ?? ''}|${task.team}`;
      const list = teams.get(key);
      if (list) list.push(task);
      else teams.set(key, [task]);
    } else {
      singles.push(task);
    }
  }
  const out: TopicItem[] = [];
  for (const [key, members] of teams) {
    const ordered = [...members].sort((a, b) => {
      const aLive = isAttentionTask(a) ? 1 : 0;
      const bLive = isAttentionTask(b) ? 1 : 0;
      if (aLive !== bLive) return bLive - aLive;
      return (taskTimestamp(b) || 0) - (taskTimestamp(a) || 0);
    });
    const primary = ordered[0];
    const teamName = primary.team ?? '';
    const active = ordered.some(isAttentionTask);
    const names = ordered.map((member) => member.name || shortId(member.task_id)).join(' · ');
    const createdMs = ordered.reduce((max, member) => {
      const ts = taskTimestamp(member);
      return Number.isFinite(ts) && ts > max ? ts : max;
    }, -Infinity);
    out.push({
      id: `team:${key}`,
      title: teamName,
      subtitle: names,
      teamCount: ordered.length,
      primaryTask: primary,
      active,
      state: primary.state,
      createdAt: primary.created_at,
      createdMs: Number.isFinite(createdMs) ? createdMs : NaN,
      threadId: `team:${key}`,
      searchText: [
        teamName,
        ...ordered.flatMap((member) => [member.name, member.goal, member.kind, member.task_id]),
      ].filter(Boolean).join(' ').toLowerCase(),
    });
  }
  for (const task of singles) {
    const title = task.name || task.goal || task.kind || task.task_id;
    const subtitle = task.name && task.goal && task.goal !== task.name ? task.goal : '';
    out.push({
      id: task.task_id,
      title,
      subtitle,
      primaryTask: task,
      active: isAttentionTask(task),
      state: task.state,
      createdAt: task.created_at,
      createdMs: taskTimestamp(task),
      threadId: delegationThreadId(task),
      searchText: [task.name, task.goal, task.team, task.kind, task.task_id].filter(Boolean).join(' ').toLowerCase(),
    });
  }
  out.sort((a, b) => {
    if (a.active !== b.active) return a.active ? -1 : 1;
    const aMs = Number.isFinite(a.createdMs) ? a.createdMs : 0;
    const bMs = Number.isFinite(b.createdMs) ? b.createdMs : 0;
    return bMs - aMs;
  });
  return out;
}

interface HierarchyProps<T extends HierarchyEntry> {
  entries: readonly T[];
  label: string;
  domain: 'artifacts' | 'tasks';
  renderLabel?: (entry: T) => React.ReactNode;
  renderMetadata: (entry: T) => React.ReactNode;
  onActivate: (entry: T) => void;
}

/** Two App domains use the same public Tree. All received Items and Groups
 * stay authored; expansion never performs fetching or materialization.
 * Selection remains controlled-empty. Public value proposals are the App's
 * activation input: branches disclose, leaves inspect/open their own record.
 * This does not admit a new portable single-selection contract.
 */
function InspectorHierarchy<T extends HierarchyEntry>({
  entries, label, domain, renderLabel, renderMetadata, onActivate,
}: HierarchyProps<T>) {
  const root = React.useRef<InspectorTreeHandle>(null);
  const alive = React.useRef(false);
  const current = React.useRef({ entries, onActivate });
  const [expanded, setExpanded] = React.useState<string[]>(() => [...new Set(
    entries.flatMap((entry) => entry.parentKey === undefined ? [] : [entry.parentKey]),
  )]);
  const presented = React.useMemo(() => presentHierarchy(entries), [entries]);
  React.useLayoutEffect(() => {
    alive.current = true;
    return () => { alive.current = false; };
  }, []);
  React.useLayoutEffect(() => { current.current = { entries, onActivate }; });
  const activate = ({ value }: { value: string }) => {
    if (!alive.current) return;
    const snapshot = current.current;
    const entry = snapshot.entries.find((item) => item.key === value);
    if (!entry) return; // A removed record cannot receive an old activation.
    if (snapshot.entries.some((item) => item.parentKey === value)) {
      const instance = root.current;
      if (!instance) return;
      const disclose = () => {
        const exposes = instance.getExposes();
        exposes.requestExpanded(value, !exposes.getExpandedKeys().includes(value));
      };
      if (instance.invokeInCallbackScope) instance.invokeInCallbackScope(disclose);
      else disclose();
    }
    snapshot.onActivate(entry);
  };
  const render = (node: PresentedHierarchy<T>): React.ReactNode => (
    <React.Fragment key={node.reactKey}>
      <TreeItem
        className="inspector-tree-item"
        styleProfile="rhea"
        appearance="embedded"
        nodeKey={node.entry.key}
        parentKey={node.entry.parentKey}
        textValue={node.entry.label}
      >
        <span className="inspector-tree-caret" aria-hidden="true">
          {node.children.length ? <Icon name="chevron" size={14} /> : <span />}
        </span>
        <span className="inspector-tree-label" data-entry-key={node.entry.key}>{renderLabel?.(node.entry) ?? node.entry.label}</span>
        <span className="inspector-tree-meta">{renderMetadata(node.entry)}</span>
      </TreeItem>
      {node.children.length > 0 && (
        <TreeGroup className="inspector-tree-group" nodeKey={node.entry.key}>
          {node.children.map(render)}
        </TreeGroup>
      )}
    </React.Fragment>
  );
  return <section className="inspector-hierarchy" data-tree-domain={domain}>
    <TreeRoot
      ref={root}
      className="inspector-tree-root"
      styleProfile="rhea"
      appearance="embedded"
      a11yLabel={label}
      value=""
      expandedKeys={expanded}
      onValueChange={activate}
      onExpandedChange={({ expandedKeys }: { expandedKeys: string[] }) => {
        if (alive.current) setExpanded([...expandedKeys]);
      }}
    >
      {presented.map(render)}
    </TreeRoot>
    <p className="inspector-tree-invalid" role="status">{t('tree.invalid')}</p>
  </section>;
}

function TaskRowItem({
  title, subtitle, badge, state, createdAt, selected, onInspect, onOpenThread,
}: {
  title: string;
  subtitle?: string;
  badge?: string;
  state: string;
  createdAt?: string;
  selected: boolean;
  onInspect: () => void;
  onOpenThread?: () => void;
}) {
  return (
    <div className="topic-row" data-selected={selected ? '' : undefined}>
      <button type="button" className="topic-row-main" onClick={onInspect}>
        <span className="task-dot" data-state={state} aria-hidden="true" />
        <span className="topic-row-body">
          <span className="topic-row-top">
            <span className="topic-row-title">{title}</span>
            {badge && <span className="topic-row-badge">{badge}</span>}
          </span>
          {subtitle && <span className="topic-row-sub">{subtitle}</span>}
        </span>
        <span className="topic-row-meta">
          <span className="task-state-text" data-state={state}>{taskStateLabel(state)}</span>
          <time title={formatStamp(createdAt)}>{formatCompactAge(createdAt)}</time>
        </span>
      </button>
      {onOpenThread && (
        <button
          type="button"
          className="topic-row-open"
          aria-label={t('inspector.openThread')}
          title={t('inspector.openThread')}
          onClick={onOpenThread}
        >
          <Icon name="chat" size={14} />
        </button>
      )}
    </div>
  );
}

export function TaskHierarchy({ tasks, onOpenThread, pendingDecision }: {
  tasks: readonly TaskSummary[];
  onOpenThread?: (threadId: string) => void;
  pendingDecision?: PendingDecisionNotice;
}) {
  const { recent, dormant, newestMs } = React.useMemo(() => partitionTasksByHorizon(tasks), [tasks]);
  const recentEntries = React.useMemo(() => taskEntries(recent), [recent]);
  const topics = React.useMemo(() => buildTopics(tasks), [tasks]);
  const activeTasks = React.useMemo(() => tasks.filter(isAttentionTask), [tasks]);

  const [scope, setScope] = React.useState<TaskScope>('recent');
  const [query, setQuery] = React.useState('');
  const [archiveOpen, setArchiveOpen] = React.useState(false);
  const [inspectedId, setInspectedId] = React.useState<string | null>(null);

  const inspected = tasks.find((task) => task.task_id === inspectedId);
  const openDelegation = inspected && onOpenThread && isDelegation(inspected)
    ? () => onOpenThread(delegationThreadId(inspected)!)
    : undefined;

  const refNow = newestMs ?? Date.now();
  const dormantBuckets = React.useMemo(
    () => groupByHorizon(dormant, (task) => taskTimestamp(task), () => false, refNow),
    [dormant, refNow],
  );
  const topicBuckets = React.useMemo(
    () => groupByHorizon(topics, (item) => item.createdMs, (item) => item.active, refNow),
    [topics, refNow],
  );
  const recentTopicBuckets = topicBuckets.filter((bucket) => !bucket.dormant);
  const dormantTopicBuckets = topicBuckets.filter((bucket) => bucket.dormant);
  const dormantTopicCount = dormantTopicBuckets.reduce((sum, bucket) => sum + bucket.items.length, 0);

  const needle = query.trim().toLowerCase();
  const searchMatches = React.useMemo(() => {
    if (!needle) return [];
    return tasks
      .filter((task) => {
        const hay = [task.name, task.goal, task.team, task.kind, task.task_id]
          .filter(Boolean).join(' ').toLowerCase();
        return hay.includes(needle);
      })
      .sort((a, b) => {
        const aLive = isAttentionTask(a) ? 1 : 0;
        const bLive = isAttentionTask(b) ? 1 : 0;
        if (aLive !== bLive) return bLive - aLive;
        return (taskTimestamp(b) || 0) - (taskTimestamp(a) || 0);
      });
  }, [tasks, needle]);

  const scopes: Array<{ id: TaskScope; label: string; count: number }> = [
    { id: 'recent', label: t('tasks.scope.recent'), count: recent.length },
    { id: 'live', label: t('tasks.scope.live'), count: activeTasks.length },
    { id: 'topics', label: t('tasks.scope.topics'), count: topics.length },
    { id: 'archive', label: t('tasks.scope.archive'), count: dormant.length },
  ];

  const renderTaskRow = (task: TaskSummary) => {
    const threadId = delegationThreadId(task);
    const title = task.name || task.goal || task.kind || task.task_id;
    const subtitle = task.name && task.goal && task.goal !== task.name ? task.goal : undefined;
    return (
      <TaskRowItem
        key={task.task_id}
        title={title}
        subtitle={subtitle}
        badge={task.team ?? undefined}
        state={task.state}
        createdAt={task.created_at}
        selected={inspectedId === task.task_id}
        onInspect={() => setInspectedId(task.task_id)}
        onOpenThread={threadId && onOpenThread ? () => onOpenThread(threadId) : undefined}
      />
    );
  };

  return <>
    <div className="tasks-controls">
      <div className="inspector-search-wrap">
        <Icon name="search" size={13} />
        <input
          type="search"
          className="inspector-search tasks-search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Escape') {
              if (query) setQuery('');
              else (event.currentTarget as HTMLInputElement).blur();
            }
          }}
          placeholder={t('tasks.searchPlaceholder')}
          aria-label={t('tasks.searchPlaceholder')}
          spellCheck={false}
        />
        {query && (
          <button
            type="button"
            className="inspector-search-clear"
            aria-label={t('files.searchClear')}
            onClick={() => setQuery('')}
          >
            <Icon name="close" size={12} />
          </button>
        )}
      </div>
      {!needle && (
        <ScopeTabs
          value={scope}
          onChange={setScope}
          label={t('config.tasks')}
          items={scopes}
        />
      )}
    </div>

    {pendingDecision && !needle && (scope === 'recent' || scope === 'live') && (
      <PendingDecisionBanner decision={pendingDecision} />
    )}

    {needle ? (
      <div className="tasks-stream" data-tasks-mode="search">
        <div className="inspector-section-bar is-quiet">
          <span className="inspector-section-stat">{t('tasks.searchHits', { count: searchMatches.length })}</span>
        </div>
        {searchMatches.length === 0
          ? <p className="tasks-empty">{t('tasks.noMatches')}</p>
          : <div className="topic-list">{searchMatches.map(renderTaskRow)}</div>}
      </div>
    ) : scope === 'recent' ? (
      <>
        <div className="inspector-section-bar">
          <span className="inspector-section-title">{t('tasks.lineageSection')}</span>
          <span className="inspector-section-stat">{t('inspector.summaryTasks', { active: activeTasks.length, total: tasks.length })}</span>
        </div>
        <InspectorHierarchy
          entries={recentEntries}
          label={t('config.tasks')}
          domain="tasks"
          renderLabel={(entry) => <>
            <span className="task-dot" data-state={entry.task.state} aria-hidden="true" />
            <span className="task-name">{entry.label}</span>
          </>}
          renderMetadata={(entry) => <>
            <span className="task-state-text" data-state={entry.task.state}>{taskStateLabel(entry.task.state)}</span>
            <time title={formatStamp(entry.task.created_at)}>{formatCompactAge(entry.task.created_at)}</time>
          </>}
          onActivate={(entry) => setInspectedId(entry.task.task_id)}
        />
        {dormant.length > 0 && (
          <section className="tasks-archive-section" aria-label={t('tasks.archiveHeader')}>
            <button
              type="button"
              className="tasks-archive-toggle"
              aria-expanded={archiveOpen}
              onClick={() => setArchiveOpen((open) => !open)}
            >
              <Icon name="chevron" size={13} />
              <span className="tasks-archive-title">{t('tasks.archiveHeader')}</span>
              <span className="tasks-archive-meta">{t('tasks.archiveMeta', { count: dormant.length })}</span>
            </button>
            {archiveOpen && (
              <div className="tasks-archive-body">
                {dormantBuckets.map((bucket) => (
                  <div className="horizon-group" key={bucket.key}>
                    <div className="horizon-head">
                      <span>{bucket.label}</span>
                      <span className="horizon-count">{bucket.items.length}</span>
                    </div>
                    <div className="topic-list">{bucket.items.map(renderTaskRow)}</div>
                  </div>
                ))}
              </div>
            )}
          </section>
        )}
      </>
    ) : scope === 'live' ? (
      <div className="tasks-stream" data-tasks-mode="live">
        {activeTasks.length === 0
          ? <p className="tasks-empty">{t('tasks.noLive')}</p>
          : <div className="topic-list">{activeTasks.map(renderTaskRow)}</div>}
      </div>
    ) : scope === 'topics' ? (
      <div className="tasks-stream" data-tasks-mode="topics">
        {recentTopicBuckets.map((bucket) => (
          <div className="horizon-group" key={bucket.key}>
            <div className="horizon-head">
              <span>{bucket.label}</span>
              <span className="horizon-count">{bucket.items.length}</span>
            </div>
            <div className="topic-list">
              {bucket.items.map((topic) => (
                <TaskRowItem
                  key={topic.id}
                  title={topic.title}
                  subtitle={topic.subtitle || undefined}
                  badge={topic.teamCount ? t('tasks.teamMembers', { count: topic.teamCount }) : undefined}
                  state={topic.state}
                  createdAt={topic.createdAt}
                  selected={inspectedId === topic.primaryTask.task_id}
                  onInspect={() => setInspectedId(topic.primaryTask.task_id)}
                  onOpenThread={topic.threadId && onOpenThread ? () => onOpenThread(topic.threadId!) : undefined}
                />
              ))}
            </div>
          </div>
        ))}
        {dormantTopicCount > 0 && (
          <section className="tasks-archive-section" aria-label={t('tasks.archiveHeader')}>
            <button
              type="button"
              className="tasks-archive-toggle"
              aria-expanded={archiveOpen}
              onClick={() => setArchiveOpen((open) => !open)}
            >
              <Icon name="chevron" size={13} />
              <span className="tasks-archive-title">{t('tasks.archiveHeader')}</span>
              <span className="tasks-archive-meta">{t('tasks.archiveMeta', { count: dormantTopicCount })}</span>
            </button>
            {archiveOpen && (
              <div className="tasks-archive-body">
                {dormantTopicBuckets.map((bucket) => (
                  <div className="horizon-group" key={bucket.key}>
                    <div className="horizon-head">
                      <span>{bucket.label}</span>
                      <span className="horizon-count">{bucket.items.length}</span>
                    </div>
                    <div className="topic-list">
                      {bucket.items.map((topic) => (
                        <TaskRowItem
                          key={topic.id}
                          title={topic.title}
                          subtitle={topic.subtitle || undefined}
                          badge={topic.teamCount ? t('tasks.teamMembers', { count: topic.teamCount }) : undefined}
                          state={topic.state}
                          createdAt={topic.createdAt}
                          selected={inspectedId === topic.primaryTask.task_id}
                          onInspect={() => setInspectedId(topic.primaryTask.task_id)}
                          onOpenThread={topic.threadId && onOpenThread ? () => onOpenThread(topic.threadId!) : undefined}
                        />
                      ))}
                    </div>
                  </div>
                ))}
              </div>
            )}
          </section>
        )}
      </div>
    ) : (
      <div className="tasks-stream" data-tasks-mode="archive">
        {dormantBuckets.length === 0
          ? <p className="tasks-empty">{t('tasks.noArchive')}</p>
          : dormantBuckets.map((bucket) => (
            <div className="horizon-group" key={bucket.key}>
              <div className="horizon-head">
                <span>{bucket.label}</span>
                <span className="horizon-count">{bucket.items.length}</span>
              </div>
              <div className="topic-list">{bucket.items.map(renderTaskRow)}</div>
            </div>
          ))}
      </div>
    )}

    {inspectedId !== null && <section className="inspector-tree-details" aria-label={t('tree.details')}>
      <header className="inspector-details-head">
        <div className="inspector-details-title">
          <h3>{inspected ? (inspected.name || inspected.kind || t('tree.details')) : t('tree.details')}</h3>
          {inspected && <span className="task-state-text" data-state={inspected.state}>{taskStateLabel(inspected.state)}</span>}
        </div>
        {openDelegation && (
          <Button variant="outline" size="sm" className="inspector-open-thread" onClick={openDelegation}>
            <Icon name="chat" size={13} />{t('inspector.openThread')}
          </Button>
        )}
      </header>
      {inspected ? <>
        {inspected.goal && <p className="inspector-details-goal">{inspected.goal}</p>}
        <dl className="facts is-compact">
          <div className="fact"><dt>{t('tree.taskIdentity')}</dt><dd className="is-mono">{inspected.task_id}</dd></div>
          <div className="fact"><dt>{t('tree.taskParent')}</dt><dd className="is-mono">{inspected.parent_task_id || '—'}</dd></div>
          <div className="fact"><dt>{t('tree.taskRole')}</dt><dd>{inspected.role}</dd></div>
          <div className="fact"><dt>{t('tree.taskKind')}</dt><dd className="is-mono">{inspected.kind}</dd></div>
          <div className="fact"><dt>{t('tree.taskCreated')}</dt><dd>{formatStamp(inspected.created_at)}</dd></div>
          {!inspected.goal && <div className="fact"><dt>{t('tree.taskGoal')}</dt><dd>—</dd></div>}
        </dl>
      </> : <p className="muted">{t('tree.recordGone')}</p>}
    </section>}
  </>;
}

export function ArtifactHierarchy({
  artifacts, onOpenWork,
}: {
  artifacts: NonNullable<PlaygroundView['artifacts']>;
  onOpenWork?: (artifactId: string) => void;
}) {
  const entries = React.useMemo(() => artifactEntries(artifacts), [artifacts]);
  return <InspectorHierarchy
    entries={entries}
    label={t('work.title')}
    domain="artifacts"
    renderLabel={(entry) => entry.kind === 'mime'
      ? <><Icon name="folder" size={14} /><span className="artifact-mime-label">{entry.mime || t('config.unknownType')}</span></>
      : <><Icon name="file" size={14} /><span>{entry.label}</span></>}
    renderMetadata={(entry) => entry.kind === 'mime'
      ? <span className="inspector-count-text">{entry.count}</span>
      : <><span className="artifact-rev-tag" data-lifecycle={String(entry.artifact.lifecycle ?? 'draft')}>r{String(entry.artifact.revision ?? '—')} · {String(entry.artifact.lifecycle ?? 'draft')}{entry.artifact.backend ? ` · ${t('work.backend')}` : ''}</span>
          <time title={formatStamp(entry.artifact.updated_at)}>{formatCompactAge(entry.artifact.updated_at)}</time></>}
    onActivate={(entry) => {
      if (entry.kind === 'artifact' && isPage(entry.artifact.mime))
        onOpenWork?.(entry.artifact.artifact_id);
    }}
  />;
}
