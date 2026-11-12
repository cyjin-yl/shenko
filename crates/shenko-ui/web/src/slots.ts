import type { IconName } from './icons';
import type { ViewName } from './projection';

/**
 * Lens contributions — the shell's first extension point
 * (docs/specs/web-interface.md#spec-21-3).
 *
 * The subject's stream is the one fixed surface. Everything else is a lens
 * that opens beside it, and the shell's own pages are registered through the
 * same registry a plugin or a work uses, so a contribution is never a
 * second-class citizen of a hard-coded navigation.
 *
 * Trust follows the source, not the registry:
 * - `shell` lenses are this bundle's own components;
 * - `work` lenses render a Playground work's published page inside a
 *   sandboxed frame. The server also sends a CSP `sandbox` for that page,
 *   so it runs in an opaque origin either way and holds none of the shell's
 *   authority; what it may change goes through the work's declared actions.
 */
export type LensSource = 'shell' | 'work';

export interface LensContribution {
  id: string;
  title: string;
  icon: IconName;
  order: number;
  source: LensSource;
  /** Shell lenses are bound to one projection view. */
  view?: ViewName;
  /** Work lenses name the artifact and the revision they were pinned at. */
  artifact_id?: string;
}

const registry = new Map<string, LensContribution>();
const listeners = new Set<() => void>();
let snapshot: LensContribution[] = [];

function publish() {
  snapshot = [...registry.values()].sort((left, right) => left.order - right.order || left.id.localeCompare(right.id));
  listeners.forEach((listener) => listener());
}

/** Register a lens; the returned function removes it (plugin unload). */
export function registerLens(contribution: LensContribution): () => void {
  registry.set(contribution.id, contribution);
  publish();
  return () => {
    if (registry.get(contribution.id) === contribution) {
      registry.delete(contribution.id);
      publish();
    }
  };
}

export function subscribeLenses(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

export function lensSnapshot(): LensContribution[] {
  return snapshot;
}

registerLens({ id: 'tasks', title: '任务', icon: 'tasks', order: 10, source: 'shell', view: 'tasks' });
registerLens({ id: 'memory', title: '记忆', icon: 'memory', order: 20, source: 'shell', view: 'memory' });
registerLens({ id: 'playground', title: 'Playground', icon: 'playground', order: 30, source: 'shell', view: 'playground' });
registerLens({ id: 'config', title: '配置', icon: 'config', order: 90, source: 'shell', view: 'config' });

/// Works the reader pinned as lenses. This is a reading preference of this
/// browser, like the theme: it does not change the work or its permissions.
const PINNED_KEY = 'shenko.lenses.pinned.v1';

let pagePinnedWorks: string[] | undefined;

export function pinnedWorks(): string[] {
  if (pagePinnedWorks) return [...pagePinnedWorks];
  try {
    const value = JSON.parse(localStorage.getItem(PINNED_KEY) ?? '[]');
    return Array.isArray(value) ? value.filter((item): item is string => typeof item === 'string') : [];
  } catch {
    return [];
  }
}

export function setWorkPinned(artifactId: string, pinned: boolean): void {
  const current = new Set(pinnedWorks());
  if (pinned) current.add(artifactId);
  else current.delete(artifactId);
  const next = [...current];
  try {
    localStorage.setItem(PINNED_KEY, JSON.stringify(next));
    pagePinnedWorks = undefined;
  } catch {
    pagePinnedWorks = next;
  }
  syncWorkLenses(lastWorks);
}

let lastWorks: Array<{ artifact_id: string; title?: string; mime?: string }> = [];
const workDisposers = new Map<string, () => void>();

/// Keep pinned, still-existing, page-bearing works registered as lenses.
/// An unavailable listing is not evidence that a work was removed.
export function syncWorkLenses(works: Array<{ artifact_id: string; title?: string; mime?: string }> | undefined): void {
  if (!works) return;
  lastWorks = works;
  const pinned = new Set(pinnedWorks());
  const wanted = new Map(works
    .filter((work) => pinned.has(work.artifact_id) && isPage(work.mime))
    .map((work) => [work.artifact_id, work]));
  for (const [id, dispose] of workDisposers) {
    if (!wanted.has(id)) {
      dispose();
      workDisposers.delete(id);
    }
  }
  let order = 40;
  for (const [id, work] of wanted) {
    const current = registry.get(`work:${id}`);
    const title = String(work.title ?? id);
    order += 1;
    if (current && current.title === title) continue;
    workDisposers.get(id)?.();
    workDisposers.set(id, registerLens({
      id: `work:${id}`, title, icon: 'file', order, source: 'work', artifact_id: id,
    }));
  }
}

export function isPage(mime: string | undefined): boolean {
  return mime === 'text/html';
}
