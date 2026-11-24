import * as React from 'react';
import { text as t } from './messages';
import type { PlaygroundView } from './projection';
import { Icon } from './icons';
import { isPage, pinnedWorks, setWorkPinned, subscribeLenses } from './slots';
import type { DemoWidgetPreview } from './threads';
import { ArtifactHierarchy } from './inspector-trees';
import { storageGet, storageSet } from './ui-preferences';
import { formatCompactAge, formatStamp } from './ui-kit';

type ArtifactRecord = NonNullable<PlaygroundView['artifacts']>[number];
type WidgetScope = 'recent' | 'pinned' | 'archive' | 'index';
type WidgetSize = 'compact' | 'wide';

const DAY_MS = 86_400_000;
const DORMANT_HORIZON_MS = 7 * DAY_MS;
const WIDGET_SIZES_KEY = 'shenko.widget.sizes.v1';
const SAFE_MODE = new URLSearchParams(location.search).get('safe') === '1';

function artifactTimestamp(artifact: ArtifactRecord): number {
  return artifact.updated_at ? Date.parse(artifact.updated_at) : NaN;
}

export function isLiveArtifact(artifact: ArtifactRecord): boolean {
  const lifecycle = String(artifact.lifecycle ?? '');
  if (lifecycle === 'archived' || lifecycle === 'stopped') return false;
  return lifecycle === 'running' || lifecycle === 'published' || Boolean(artifact.active_manifest_digest) || artifact.backend === true;
}

function isArchivedArtifact(artifact: ArtifactRecord, newestMs: number | null, pinnedSet: ReadonlySet<string>): boolean {
  if (String(artifact.lifecycle ?? '') === 'archived') return true;
  if (pinnedSet.has(artifact.artifact_id) || isLiveArtifact(artifact)) return false;
  if (newestMs === null) return false;
  const ts = artifactTimestamp(artifact);
  return Number.isFinite(ts) && (newestMs - ts) > DORMANT_HORIZON_MS;
}

function archiveHorizonLabel(updatedMs: number, newestMs: number): { key: string; label: string } {
  if (!Number.isFinite(updatedMs)) return { key: 'older', label: t('tasks.bucket.older') };
  const delta = Math.max(0, newestMs - updatedMs);
  if (delta <= 30 * DAY_MS) return { key: 'month', label: t('tasks.bucket.month') };
  const months = Math.max(1, Math.floor(delta / (30 * DAY_MS)));
  if (months < 12) return { key: `mo:${months}`, label: t('tasks.bucket.monthsAgo', { months }) };
  return { key: 'older', label: t('tasks.bucket.older') };
}

function groupArchived(items: readonly ArtifactRecord[], newestMs: number): Array<{ key: string; label: string; items: ArtifactRecord[] }> {
  const buckets = new Map<string, { key: string; label: string; items: ArtifactRecord[] }>();
  for (const item of items) {
    const info = archiveHorizonLabel(artifactTimestamp(item), newestMs);
    const existing = buckets.get(info.key);
    if (existing) existing.items.push(item);
    else buckets.set(info.key, { ...info, items: [item] });
  }
  return [...buckets.values()];
}

function readWidgetSizes(): Record<string, WidgetSize> {
  try {
    const raw = storageGet(WIDGET_SIZES_KEY);
    if (!raw) return {};
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return {};
    const clean: Record<string, WidgetSize> = {};
    for (const [id, size] of Object.entries(parsed)) {
      if (size === 'compact' || size === 'wide') clean[id] = size;
    }
    return clean;
  } catch {
    return {};
  }
}

function resolveGridSizes(
  items: readonly ArtifactRecord[],
  overrides: Readonly<Record<string, WidgetSize>>,
): WidgetSize[] {
  const sizes: WidgetSize[] = [];
  let colsUsed = 0;
  items.forEach((artifact, index) => {
    const custom = overrides[artifact.artifact_id];
    let size: WidgetSize;
    if (custom) {
      size = custom;
    } else if (items.length === 1) {
      size = 'wide';
    } else if (index === 0 && isPage(artifact.mime) && Boolean(artifact.active_manifest_digest)) {
      size = 'wide';
    } else if (index === items.length - 1 && colsUsed % 2 === 0) {
      size = 'wide';
    } else {
      size = 'compact';
    }
    sizes.push(size);
    colsUsed += size === 'wide' ? 2 : 1;
  });
  return sizes;
}

function shortMimeLabel(mime: string | undefined): string {
  if (!mime) return t('config.unknownType');
  if (mime === 'text/html') return 'HTML';
  if (mime === 'text/markdown') return 'Markdown';
  if (mime === 'application/json') return 'JSON';
  if (mime === 'text/plain') return 'Text';
  if (mime === 'image/svg+xml') return 'SVG';
  return mime;
}

function WidgetTile({
  artifact,
  size,
  pinned,
  demo,
  theme,
  preview,
  onTogglePin,
  onToggleSize,
  onOpen,
}: {
  artifact: ArtifactRecord;
  size: WidgetSize;
  pinned: boolean;
  demo: boolean;
  theme: 'light' | 'dark';
  preview?: DemoWidgetPreview;
  onTogglePin: () => void;
  onToggleSize: () => void;
  onOpen?: () => void;
}) {
  const title = String(artifact.title || artifact.artifact_id);
  const page = isPage(artifact.mime);
  const lifecycle = String(artifact.lifecycle ?? 'draft');
  const liveAdopted = !demo && !SAFE_MODE && page && Boolean(artifact.active_manifest_digest);
  const demoHtml = demo && !SAFE_MODE && preview?.kind === 'html' ? preview.html : undefined;
  const demoMarkdown = demo && preview?.kind === 'markdown' ? preview.text : undefined;
  const hasLiveCanvas = liveAdopted || Boolean(demoHtml);
  const metaParts = [
    artifact.revision !== undefined ? `r${artifact.revision}` : null,
    lifecycle,
    artifact.backend ? t('work.backend') : null,
    !hasLiveCanvas ? shortMimeLabel(artifact.mime) : null,
  ].filter(Boolean);

  return (
    <article
      className="widget-tile"
      data-artifact-id={artifact.artifact_id}
      data-size={size}
      data-lifecycle={lifecycle}
      data-pinned={pinned ? '' : undefined}
    >
      <header className="widget-head">
        <span className="widget-dot" data-lifecycle={lifecycle} aria-hidden="true" />
        <span className="widget-title" title={title}>{title}</span>
        <time className="widget-age" title={formatStamp(artifact.updated_at)}>
          {formatCompactAge(artifact.updated_at)}
        </time>
        <div className="widget-actions">
          <button
            type="button"
            className="widget-act"
            data-active={pinned ? '' : undefined}
            aria-pressed={pinned}
            aria-label={pinned ? t('widgets.unpin') : t('widgets.pin')}
            title={pinned ? t('widgets.unpin') : t('widgets.pin')}
            onClick={onTogglePin}
          >
            <Icon name="pin" size={13} />
          </button>
          <button
            type="button"
            className="widget-act"
            aria-label={size === 'wide' ? t('widgets.compact') : t('widgets.wider')}
            title={size === 'wide' ? t('widgets.compact') : t('widgets.wider')}
            onClick={onToggleSize}
          >
            <Icon name="resize" size={13} />
          </button>
          {onOpen && (
            <button
              type="button"
              className="widget-act"
              aria-label={t('widgets.open')}
              title={t('widgets.open')}
              onClick={onOpen}
            >
              <Icon name="open" size={13} />
            </button>
          )}
        </div>
      </header>

      <div className={`widget-body${hasLiveCanvas ? ' is-frame' : ''}`}>
        {liveAdopted ? (
          <iframe
            key={`live:${artifact.artifact_id}@${artifact.active_manifest_digest ?? artifact.revision ?? 0}`}
            className="widget-live-frame"
            src={`/playground/${encodeURIComponent(artifact.artifact_id)}/adopted`}
            title={title}
            loading="lazy"
          />
        ) : demoHtml ? (
          <iframe
            key={`demo:${artifact.artifact_id}:${theme}@${artifact.revision ?? 0}`}
            className="widget-live-frame"
            sandbox=""
            srcDoc={demoHtml}
            title={title}
          />
        ) : demoMarkdown ? (
          <p className="widget-markdown">{demoMarkdown}</p>
        ) : SAFE_MODE && page && artifact.active_manifest_digest ? (
          <div className="widget-fallback">
            <span className="widget-fallback-hint">{t('widgets.safeModeHint')}</span>
          </div>
        ) : (
          <div className="widget-fallback">
            <span className="widget-fallback-hint">
              {page && !artifact.active_manifest_digest ? t('widgets.draftHint') : shortMimeLabel(artifact.mime)}
            </span>
            {onOpen && (
              <button type="button" className="widget-open-link" onClick={onOpen}>
                {t('common.open')}
              </button>
            )}
          </div>
        )}
      </div>

      <footer className="widget-foot">
        <span>{metaParts.join(' · ')}</span>
      </footer>
    </article>
  );
}

/**
 * Dynamic, template-free persistent Widgets board for Shenko Playground.
 * Shenko creates arbitrary works from mood, thoughts, sightings, or tasks;
 * this board lets live adopted widgets exist persistently, separates Pinned /
 * Recent / Live / Archive horizons, supports instant search across all time,
 * and preserves the public Proto-UI ArtifactHierarchy index beneath the board.
 */
export function PlaygroundWidgets({
  artifacts,
  ownerKey,
  demo,
  theme,
  getDemoPreview,
  onOpenWork,
  onInspectWork,
}: {
  artifacts: readonly ArtifactRecord[];
  ownerKey: string;
  demo: boolean;
  theme: 'light' | 'dark';
  getDemoPreview?: (artifactId: string, theme: 'light' | 'dark') => DemoWidgetPreview | undefined;
  onOpenWork?: (artifactId: string) => void;
  onInspectWork?: (artifactId: string) => void;
}) {
  const [pinnedIds, setPinnedIds] = React.useState<string[]>(() => pinnedWorks());
  const initialHasLive = artifacts.some(isLiveArtifact) || artifacts.some((item) => pinnedIds.includes(item.artifact_id));
  const [scope, setScope] = React.useState<WidgetScope>(() => (initialHasLive ? 'recent' : 'index'));
  const userPickedScope = React.useRef(false);
  const [query, setQuery] = React.useState('');
  const [archiveOpen, setArchiveOpen] = React.useState(false);
  const [sizeOverrides, setSizeOverrides] = React.useState<Record<string, WidgetSize>>(() => readWidgetSizes());

  React.useEffect(() => subscribeLenses(() => setPinnedIds(pinnedWorks())), []);

  const pinnedSet = React.useMemo(() => new Set(pinnedIds), [pinnedIds]);

  const { pinned, recent, live, archived, newestMs } = React.useMemo(() => {
    let newest: number | null = null;
    for (const artifact of artifacts) {
      const ts = artifactTimestamp(artifact);
      if (Number.isFinite(ts) && (newest === null || ts > newest)) newest = ts;
    }
    const pinnedList: ArtifactRecord[] = [];
    const recentList: ArtifactRecord[] = [];
    const liveList: ArtifactRecord[] = [];
    const archivedList: ArtifactRecord[] = [];
    for (const artifact of artifacts) {
      if (isLiveArtifact(artifact)) liveList.push(artifact);
      if (pinnedSet.has(artifact.artifact_id)) {
        pinnedList.push(artifact);
      } else if (isArchivedArtifact(artifact, newest, pinnedSet)) {
        archivedList.push(artifact);
      } else {
        recentList.push(artifact);
      }
    }
    archivedList.sort((a, b) => (artifactTimestamp(b) || 0) - (artifactTimestamp(a) || 0));
    return { pinned: pinnedList, recent: recentList, live: liveList, archived: archivedList, newestMs: newest };
  }, [artifacts, pinnedSet]);

  const hasLiveBoard = live.length > 0 || pinned.length > 0;
  React.useEffect(() => {
    if (!userPickedScope.current && hasLiveBoard && scope === 'index') {
      setScope('recent');
    }
  }, [hasLiveBoard, scope]);

  const selectScope = (next: WidgetScope) => {
    userPickedScope.current = true;
    setScope(next);
  };

  const refNow = newestMs ?? Date.now();
  const archivedBuckets = React.useMemo(() => groupArchived(archived, refNow), [archived, refNow]);
  const needle = query.trim().toLowerCase();
  const searchMatches = React.useMemo(() => {
    if (!needle) return [];
    return artifacts.filter((artifact) => {
      const hay = [artifact.title, artifact.artifact_id, artifact.mime, artifact.lifecycle]
        .filter(Boolean).join(' ').toLowerCase();
      return hay.includes(needle);
    });
  }, [artifacts, needle]);

  const togglePin = (artifactId: string) => {
    const nextPinned = !pinnedSet.has(artifactId);
    setWorkPinned(artifactId, nextPinned);
    setPinnedIds(pinnedWorks());
  };

  const toggleSize = (artifactId: string, current: WidgetSize) => {
    const next: WidgetSize = current === 'wide' ? 'compact' : 'wide';
    setSizeOverrides((prev) => {
      const updated = { ...prev, [artifactId]: next };
      storageSet(WIDGET_SIZES_KEY, JSON.stringify(updated));
      return updated;
    });
  };

  const openActionFor = (artifact: ArtifactRecord) => {
    if (isPage(artifact.mime) && onOpenWork) return () => onOpenWork(artifact.artifact_id);
    if (onInspectWork) return () => onInspectWork(artifact.artifact_id);
    return undefined;
  };

  const renderGrid = (items: readonly ArtifactRecord[]) => {
    const sizes = resolveGridSizes(items, sizeOverrides);
    return (
      <div className="widgets-grid">
        {items.map((artifact, index) => (
          <WidgetTile
            key={artifact.artifact_id}
            artifact={artifact}
            size={sizes[index]}
            pinned={pinnedSet.has(artifact.artifact_id)}
            demo={demo}
            theme={theme}
            preview={demo ? getDemoPreview?.(artifact.artifact_id, theme) : undefined}
            onTogglePin={() => togglePin(artifact.artifact_id)}
            onToggleSize={() => toggleSize(artifact.artifact_id, sizes[index])}
            onOpen={openActionFor(artifact)}
          />
        ))}
      </div>
    );
  };

  const scopes: Array<{ id: WidgetScope; label: string; count: number }> = [
    { id: 'recent', label: t('widgets.scope.recent'), count: pinned.length + recent.length },
    { id: 'pinned', label: t('widgets.scope.pinned'), count: pinned.length },
    { id: 'archive', label: t('widgets.scope.archive'), count: archived.length },
    { id: 'index', label: t('widgets.scope.index'), count: artifacts.length },
  ];

  return <>
    <div className="widgets-controls">
      <div className="inspector-search-wrap">
        <Icon name="search" size={13} />
        <input
          type="search"
          className="inspector-search widgets-search"
          value={query}
          onChange={(event) => setQuery(event.target.value)}
          onKeyDown={(event) => {
            if (event.key === 'Escape') {
              if (query) setQuery('');
              else (event.currentTarget as HTMLInputElement).blur();
            }
          }}
          placeholder={t('widgets.searchPlaceholder')}
          aria-label={t('widgets.searchPlaceholder')}
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
        <div className="scope-bar" role="tablist" aria-label="Playground">
          {scopes.map((item) => (
            <button
              key={item.id}
              type="button"
              role="tab"
              aria-selected={scope === item.id}
              className="scope-item"
              data-scope={item.id}
              data-active={scope === item.id ? '' : undefined}
              onClick={() => selectScope(item.id)}
            >
              <span>{item.label}</span>
              {item.count > 0 && <span className="scope-count">{item.count}</span>}
            </button>
          ))}
        </div>
      )}
    </div>

    {needle ? (
      <div className="widgets-board" data-widgets-mode="search">
        <div className="inspector-section-bar is-quiet">
          <span className="inspector-section-stat">{t('widgets.searchHits', { count: searchMatches.length })}</span>
        </div>
        {searchMatches.length === 0
          ? <p className="tasks-empty">{t('widgets.noMatches')}</p>
          : renderGrid(searchMatches)}
      </div>
    ) : scope === 'recent' ? (
      <div className="widgets-board" data-widgets-mode="recent">
        {pinned.length > 0 && (
          <section className="widgets-section" aria-label={t('widgets.section.pinned')}>
            <div className="horizon-head">
              <span>{t('widgets.section.pinned')}</span>
              <span className="horizon-count">{pinned.length}</span>
            </div>
            {renderGrid(pinned)}
          </section>
        )}
        {recent.length > 0 && (
          <section className="widgets-section" aria-label={t('widgets.section.recent')}>
            {pinned.length > 0 && (
              <div className="horizon-head">
                <span>{t('widgets.section.recent')}</span>
                <span className="horizon-count">{recent.length}</span>
              </div>
            )}
            {renderGrid(recent)}
          </section>
        )}
        {archived.length > 0 && (
          <section className="tasks-archive-section" aria-label={t('widgets.section.archive')}>
            <button
              type="button"
              className="tasks-archive-toggle widgets-archive-toggle"
              aria-expanded={archiveOpen}
              onClick={() => setArchiveOpen((open) => !open)}
            >
              <Icon name="chevron" size={13} />
              <span className="tasks-archive-title">{t('widgets.section.archive')}</span>
              <span className="tasks-archive-meta">{t('widgets.archiveMeta', { count: archived.length })}</span>
            </button>
            {archiveOpen && (
              <div className="tasks-archive-body">
                {archivedBuckets.map((bucket) => (
                  <div className="horizon-group" key={bucket.key}>
                    <div className="horizon-head">
                      <span>{bucket.label}</span>
                      <span className="horizon-count">{bucket.items.length}</span>
                    </div>
                    {renderGrid(bucket.items)}
                  </div>
                ))}
              </div>
            )}
          </section>
        )}
      </div>
    ) : scope === 'pinned' ? (
      <div className="widgets-board" data-widgets-mode="pinned">
        {pinned.length === 0
          ? <p className="tasks-empty">{t('widgets.noPinned')}</p>
          : renderGrid(pinned)}
      </div>
    ) : scope === 'archive' ? (
      <div className="widgets-board" data-widgets-mode="archive">
        {archivedBuckets.length === 0
          ? <p className="tasks-empty">{t('widgets.noArchive')}</p>
          : archivedBuckets.map((bucket) => (
            <div className="horizon-group" key={bucket.key}>
              <div className="horizon-head">
                <span>{bucket.label}</span>
                <span className="horizon-count">{bucket.items.length}</span>
              </div>
              {renderGrid(bucket.items)}
            </div>
          ))}
      </div>
    ) : (
      <div className="widgets-board" data-widgets-mode="index">
        <ArtifactHierarchy key={ownerKey} artifacts={artifacts as NonNullable<PlaygroundView['artifacts']>} onOpenWork={onOpenWork} />
      </div>
    )}
  </>;
}
