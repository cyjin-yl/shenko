import type { PlaygroundView, TaskSummary } from './projection';

export interface HierarchyEntry {
  key: string;
  parentKey?: string;
  label: string;
}
export interface PresentedHierarchy<T extends HierarchyEntry> {
  entry: T;
  reactKey: string;
  children: PresentedHierarchy<T>[];
}
export type ArtifactRecord = NonNullable<PlaygroundView['artifacts']>[number];
export type ArtifactEntry = HierarchyEntry & (
  | { kind: 'mime'; mime: string; count: number }
  | { kind: 'artifact'; artifact: ArtifactRecord }
);
export type TaskEntry = HierarchyEntry & { kind: 'task'; task: TaskSummary };

/** Application organization only: the received MIME value names a category.
 * parent_revision is a revision fact; it is never interpreted as a parent ID.
 * Duplicate and empty record identities remain duplicate/empty for Tree's
 * public invalid graph owner to reject.
 */
export function artifactEntries(artifacts: readonly ArtifactRecord[]): ArtifactEntry[] {
  const groups = new Map<string, ArtifactRecord[]>();
  for (const artifact of artifacts) {
    const mime = artifact.mime || '';
    const group = groups.get(mime);
    if (group) group.push(artifact);
    else groups.set(mime, [artifact]);
  }
  const entries: ArtifactEntry[] = [];
  for (const [mime, records] of groups) {
    const key = 'mime:' + mime;
    entries.push({ key, label: mime, kind: 'mime', mime, count: records.length });
    for (const artifact of records) entries.push({
      key: artifact.artifact_id ? 'artifact:' + artifact.artifact_id : '',
      parentKey: key,
      label: artifact.title || artifact.artifact_id || '',
      kind: 'artifact', artifact,
    });
  }
  return entries;
}

/** The server's explicit parent_task_id is the only task relationship input. */
export function taskEntries(tasks: readonly TaskSummary[]): TaskEntry[] {
  return tasks.map((task) => ({
    key: task.task_id ? 'task:' + task.task_id : '',
    ...(task.parent_task_id ? { parentKey: 'task:' + task.parent_task_id } : {}),
    label: task.name || task.goal || task.kind || task.task_id || '',
    kind: 'task' as const, task,
  }));
}

/** Arrange authored DOM without repairing the graph. Every received record is
 * materialized once, with its exact original key and parentKey, including
 * duplicate, orphan and cyclic input. Public Tree validates those facts.
 * The visited index only bounds recursive DOM construction; it grants no
 * hierarchy validity, expanded state, selection, focus or relationship ID.
 */
export function presentHierarchy<T extends HierarchyEntry>(
  entries: readonly T[],
): PresentedHierarchy<T>[] {
  const indexes = new Map<string, number[]>();
  const children = new Map<string, number[]>();
  entries.forEach((entry, index) => {
    indexes.set(entry.key, [...(indexes.get(entry.key) || []), index]);
    if (entry.parentKey !== undefined)
      children.set(entry.parentKey, [...(children.get(entry.parentKey) || []), index]);
  });
  const visited = new Set<number>();
  const visit = (index: number): PresentedHierarchy<T> | null => {
    if (visited.has(index)) return null;
    visited.add(index);
    const entry = entries[index];
    const result: PresentedHierarchy<T> = {
      entry,
      reactKey: entry.key && indexes.get(entry.key)?.length === 1
        ? entry.key : 'invalid-record:' + index,
      children: [],
    };
    for (const childIndex of children.get(entry.key) || []) {
      const child = visit(childIndex);
      if (child) result.children.push(child);
    }
    return result;
  };
  const result: PresentedHierarchy<T>[] = [];
  entries.forEach((entry, index) => {
    if (entry.parentKey === undefined || !indexes.has(entry.parentKey)) {
      const root = visit(index);
      if (root) result.push(root);
    }
  });
  // A cycle has no authored root. Keep its actual parent facts visible to
  // Tree, instead of dropping records or inventing a valid root relationship.
  entries.forEach((_, index) => {
    const root = visit(index);
    if (root) result.push(root);
  });
  return result;
}
