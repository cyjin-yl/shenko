import * as React from 'react';
import { text as t } from './messages';
import type { PlaygroundView, TaskSummary } from './projection';
import { Icon } from './icons';
import { isPage } from './slots';
import { taskStateLabel } from './threads';
import {
  TreeRoot, TreeItem, TreeGroup, type InspectorTreeHandle, formatStamp,
} from './ui-kit';
import {
  artifactEntries, taskEntries, presentHierarchy,
  type HierarchyEntry, type PresentedHierarchy,
} from './inspector-tree-model';

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

export function TaskHierarchy({ tasks }: { tasks: readonly TaskSummary[] }) {
  const entries = React.useMemo(() => taskEntries(tasks), [tasks]);
  const [inspectedId, setInspectedId] = React.useState<string | null>(null);
  const inspected = tasks.find((task) => task.task_id === inspectedId);
  return <>
    <InspectorHierarchy
      entries={entries}
      label={t('config.tasks')}
      domain="tasks"
      renderMetadata={(entry) => <><span data-state={entry.task.state}>{taskStateLabel(entry.task.state)}</span><time>{formatStamp(entry.task.created_at)}</time></>}
      onActivate={(entry) => setInspectedId(entry.task.task_id)}
    />
    {inspectedId !== null && <section className="inspector-tree-details" aria-label={t('tree.details')}>
      <h3>{t('tree.details')}</h3>
      {inspected ? <dl className="facts is-compact">
        <div className="fact"><dt>{t('tree.taskIdentity')}</dt><dd className="is-mono">{inspected.task_id}</dd></div>
        <div className="fact"><dt>{t('tree.taskParent')}</dt><dd className="is-mono">{inspected.parent_task_id || '—'}</dd></div>
        <div className="fact"><dt>{t('tree.taskRole')}</dt><dd>{inspected.role}</dd></div>
        <div className="fact"><dt>{t('tree.taskKind')}</dt><dd>{inspected.kind}</dd></div>
        <div className="fact"><dt>{t('tree.taskCreated')}</dt><dd>{formatStamp(inspected.created_at)}</dd></div>
        <div className="fact"><dt>{t('tree.taskGoal')}</dt><dd>{inspected.goal || '—'}</dd></div>
      </dl> : <p className="muted">{t('tree.recordGone')}</p>}
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
      ? entry.mime || t('config.unknownType')
      : <><Icon name="file" size={15} /><span>{entry.label}</span></>}
    renderMetadata={(entry) => entry.kind === 'mime'
      ? String(entry.count)
      : <><span>r{String(entry.artifact.revision ?? '—')} · {String(entry.artifact.lifecycle ?? 'draft')} · {entry.artifact.backend ? t('work.backend') : t('work.content')}</span>
          <time>{formatStamp(entry.artifact.updated_at)}</time></>}
    onActivate={(entry) => {
      if (entry.kind === 'artifact' && isPage(entry.artifact.mime))
        onOpenWork?.(entry.artifact.artifact_id);
    }}
  />;
}
