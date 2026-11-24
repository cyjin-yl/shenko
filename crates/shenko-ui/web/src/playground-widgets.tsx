import * as React from 'react';
import { text as t } from './messages';
import type { PlaygroundView } from './projection';
import { Icon } from './icons';
import { isPage, pinnedWorks, setWorkPinned, subscribeLenses } from './slots';
import type { DemoWidgetPreview } from './threads';
import { ArtifactHierarchy } from './inspector-trees';
import { storageGet, storageSet } from './ui-preferences';
import {
  CardContent,
  CardFooter,
  CardHeader,
  CardRoot,
  ContextMenuContent,
  ContextMenuItem,
  ContextMenuRoot,
  ContextMenuTrigger,
  DropdownContent,
  DropdownGroup,
  DropdownItem,
  DropdownLabel,
  DropdownRoot,
  DropdownSeparator,
  DropdownTrigger,
  ScopeTabs,
  formatCompactAge,
  formatStamp,
} from './ui-kit';

type ArtifactRecord = NonNullable<PlaygroundView['artifacts']>[number];
type WidgetScope = 'recent' | 'pinned' | 'archive' | 'index';

export interface WidgetSpan {
  cols: number;
  rows: number;
}

interface WidgetBoardPrefs {
  spans: Record<string, WidgetSpan>;
  order: string[];
}

const DAY_MS = 86_400_000;
const DORMANT_HORIZON_MS = 7 * DAY_MS;
const WIDGET_LAYOUT_KEY = 'shenko.widget.layout.v2';
const ROW_UNIT_PX = 70;
const GRID_GAP_PX = 10;
const SAFE_MODE = new URLSearchParams(location.search).get('safe') === '1';

const SIZE_PRESETS: ReadonlyArray<{ id: string; labelKey: 'widgets.size.strip' | 'widgets.size.square' | 'widgets.size.tall' | 'widgets.size.wide' | 'widgets.size.large'; span: WidgetSpan }> = [
  { id: 'strip', labelKey: 'widgets.size.strip', span: { cols: 4, rows: 1 } },
  { id: 'square', labelKey: 'widgets.size.square', span: { cols: 2, rows: 2 } },
  { id: 'tall', labelKey: 'widgets.size.tall', span: { cols: 2, rows: 3 } },
  { id: 'wide', labelKey: 'widgets.size.wide', span: { cols: 4, rows: 2 } },
  { id: 'large', labelKey: 'widgets.size.large', span: { cols: 4, rows: 3 } },
];

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

function clampSpan(span: WidgetSpan, maxCols = 6): WidgetSpan {
  return {
    cols: Math.max(1, Math.min(maxCols, Math.round(span.cols))),
    rows: Math.max(1, Math.min(5, Math.round(span.rows))),
  };
}

function readBoardPrefs(): WidgetBoardPrefs {
  try {
    const raw = storageGet(WIDGET_LAYOUT_KEY);
    if (!raw) return { spans: {}, order: [] };
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== 'object') return { spans: {}, order: [] };
    const spans: Record<string, WidgetSpan> = {};
    if (parsed.spans && typeof parsed.spans === 'object') {
      for (const [id, entry] of Object.entries(parsed.spans)) {
        const value = entry as Partial<WidgetSpan> | null;
        if (value && typeof value.cols === 'number' && typeof value.rows === 'number') {
          spans[id] = clampSpan({ cols: value.cols, rows: value.rows });
        }
      }
    }
    const order = Array.isArray(parsed.order)
      ? parsed.order.filter((id: unknown): id is string => typeof id === 'string')
      : [];
    return { spans, order };
  } catch {
    return { spans: {}, order: [] };
  }
}

function saveBoardPrefs(prefs: WidgetBoardPrefs): void {
  storageSet(WIDGET_LAYOUT_KEY, JSON.stringify(prefs));
}

function sortByOrder(items: readonly ArtifactRecord[], order: readonly string[]): ArtifactRecord[] {
  if (order.length === 0) return [...items];
  const rank = new Map(order.map((id, index) => [id, index]));
  return [...items].sort((a, b) => {
    const ra = rank.get(a.artifact_id);
    const rb = rank.get(b.artifact_id);
    if (ra !== undefined && rb !== undefined) return ra - rb;
    if (ra !== undefined) return -1;
    if (rb !== undefined) return 1;
    return 0;
  });
}

/**
 * Assign organic asymmetric default footprints when the user has not resized a
 * widget yet: backend widgets start wide/square, freeform HTML works vary
 * across tall portrait, square and slim strip shapes, and solo items span wide.
 */
function defaultSpansFor(items: readonly ArtifactRecord[]): WidgetSpan[] {
  if (items.length === 1) {
    return [isPage(items[0].mime) ? { cols: 6, rows: 2 } : { cols: 6, rows: 2 }];
  }
  let backendSeen = 0;
  let freeHtmlSeen = 0;
  const freeHtmlCycle: readonly WidgetSpan[] = [
    { cols: 2, rows: 3 },
    { cols: 2, rows: 2 },
    { cols: 4, rows: 1 },
    { cols: 4, rows: 2 },
  ];
  return items.map((artifact) => {
    if (isPage(artifact.mime) && artifact.backend) {
      backendSeen += 1;
      return backendSeen === 1 ? { cols: 4, rows: 2 } : { cols: 2, rows: 2 };
    }
    if (isPage(artifact.mime)) {
      const pick = freeHtmlCycle[freeHtmlSeen % freeHtmlCycle.length];
      freeHtmlSeen += 1;
      return pick;
    }
    return { cols: 2, rows: 2 };
  });
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

function WidgetSlot({
  artifact,
  span,
  pinned,
  demo,
  theme,
  preview,
  hasCustomLayout,
  isDropTarget,
  onTogglePin,
  onSetSpan,
  onMoveFront,
  onResetLayout,
  onOpen,
  onDragStartId,
  onDragOverId,
  onDropOnId,
  onDragEnd,
  onResizeState,
}: {
  artifact: ArtifactRecord;
  span: WidgetSpan;
  pinned: boolean;
  demo: boolean;
  theme: 'light' | 'dark';
  preview?: DemoWidgetPreview;
  hasCustomLayout: boolean;
  isDropTarget: boolean;
  onTogglePin: () => void;
  onSetSpan: (next: WidgetSpan) => void;
  onMoveFront: () => void;
  onResetLayout: () => void;
  onOpen?: () => void;
  onDragStartId: (id: string) => void;
  onDragOverId: (id: string | null) => void;
  onDropOnId: (targetId: string) => void;
  onDragEnd: () => void;
  onResizeState: (active: boolean) => void;
}) {
  const slotRef = React.useRef<HTMLDivElement>(null);
  const [liveSpan, setLiveSpan] = React.useState<WidgetSpan | null>(null);
  const activeSpan = liveSpan ?? span;
  const title = String(artifact.title || artifact.artifact_id);
  const page = isPage(artifact.mime);
  const lifecycle = String(artifact.lifecycle ?? 'draft');
  const liveAdopted = !demo && !SAFE_MODE && page && Boolean(artifact.active_manifest_digest);
  const demoHtml = demo && !SAFE_MODE && preview?.kind === 'html' ? preview.html : undefined;
  const demoMarkdown = demo && preview?.kind === 'markdown' ? preview.text : undefined;
  const hasLiveCanvas = liveAdopted || Boolean(demoHtml);
  const isSlim = activeSpan.rows === 1;
  const metaParts = [
    artifact.revision !== undefined ? `r${artifact.revision}` : null,
    lifecycle,
    artifact.backend ? t('work.backend') : null,
    !hasLiveCanvas ? shortMimeLabel(artifact.mime) : null,
  ].filter(Boolean);

  const beginCornerResize = (event: React.PointerEvent<HTMLButtonElement>) => {
    event.preventDefault();
    event.stopPropagation();
    const slot = slotRef.current;
    const grid = slot?.parentElement;
    if (!slot || !grid) return;
    const gridStyle = getComputedStyle(grid);
    const maxCols = Math.max(2, parseInt(gridStyle.getPropertyValue('--grid-cols') || '4', 10) || 4);
    const gridWidth = grid.getBoundingClientRect().width;
    const colStride = (gridWidth + GRID_GAP_PX) / maxCols;
    const rowStride = ROW_UNIT_PX + GRID_GAP_PX;
    const startX = event.clientX;
    const startY = event.clientY;
    const origin = { cols: Math.min(span.cols, maxCols), rows: span.rows };
    let latest = origin;
    onResizeState(true);
    setLiveSpan(origin);

    const onMove = (moveEvent: PointerEvent) => {
      const dx = moveEvent.clientX - startX;
      const dy = moveEvent.clientY - startY;
      const next = clampSpan({
        cols: origin.cols + Math.round(dx / Math.max(40, colStride)),
        rows: origin.rows + Math.round(dy / rowStride),
      }, maxCols);
      if (next.cols !== latest.cols || next.rows !== latest.rows) {
        latest = next;
        setLiveSpan(next);
      }
    };
    const finish = () => {
      window.removeEventListener('pointermove', onMove);
      window.removeEventListener('pointerup', finish);
      window.removeEventListener('pointercancel', finish);
      onResizeState(false);
      setLiveSpan(null);
      if (latest.cols !== span.cols || latest.rows !== span.rows) onSetSpan(latest);
    };
    window.addEventListener('pointermove', onMove);
    window.addEventListener('pointerup', finish);
    window.addEventListener('pointercancel', finish);
  };

  const renderMenuItems = (Item: typeof ContextMenuItem | typeof DropdownItem, withSections: boolean) => (
    <>
      {onOpen && (
        <Item styleProfile="rhea" value="open" textValue={t('widgets.open')} className="widget-menu-item" onSelect={onOpen}>
          <Icon name="open" size={13} />
          <span>{t('widgets.open')}</span>
        </Item>
      )}
      <Item
        styleProfile="rhea"
        value="pin"
        textValue={pinned ? t('widgets.unpin') : t('widgets.pin')}
        className="widget-menu-item"
        onSelect={onTogglePin}
      >
        <Icon name="pin" size={13} />
        <span>{pinned ? t('widgets.unpin') : t('widgets.pin')}</span>
      </Item>
      {withSections ? (
        <>
          <DropdownSeparator styleProfile="rhea" className="widget-menu-sep" />
          <DropdownGroup styleProfile="rhea" className="widget-menu-group">
            <DropdownLabel styleProfile="rhea" className="widget-menu-label">{t('widgets.sizeGroup')}</DropdownLabel>
            {SIZE_PRESETS.map((preset) => (
              <Item
                key={preset.id}
                styleProfile="rhea"
                value={`size:${preset.id}`}
                textValue={t(preset.labelKey)}
                className="widget-menu-item"
                onSelect={() => onSetSpan(preset.span)}
              >
                <Icon name="resize" size={13} />
                <span>{t(preset.labelKey)}</span>
              </Item>
            ))}
          </DropdownGroup>
          <DropdownSeparator styleProfile="rhea" className="widget-menu-sep" />
        </>
      ) : (
        <>
          <div className="widget-menu-sep" role="separator" />
          <div className="widget-menu-label">{t('widgets.sizeGroup')}</div>
          {SIZE_PRESETS.map((preset) => (
            <Item
              key={preset.id}
              styleProfile="rhea"
              value={`size:${preset.id}`}
              textValue={t(preset.labelKey)}
              className="widget-menu-item"
              onSelect={() => onSetSpan(preset.span)}
            >
              <Icon name="resize" size={13} />
              <span>{t(preset.labelKey)}</span>
            </Item>
          ))}
          <div className="widget-menu-sep" role="separator" />
        </>
      )}
      <Item styleProfile="rhea" value="front" textValue={t('widgets.moveFront')} className="widget-menu-item" onSelect={onMoveFront}>
        <Icon name="chevron" size={13} />
        <span>{t('widgets.moveFront')}</span>
      </Item>
      {hasCustomLayout && (
        <Item styleProfile="rhea" value="reset" textValue={t('widgets.resetLayout')} className="widget-menu-item" onSelect={onResetLayout}>
          <Icon name="undo" size={13} />
          <span>{t('widgets.resetLayout')}</span>
        </Item>
      )}
    </>
  );

  return (
    <div
      ref={slotRef}
      className="widget-slot"
      style={{
        ['--cols' as string]: String(activeSpan.cols),
        ['--rows' as string]: String(activeSpan.rows),
      }}
      data-artifact-id={artifact.artifact_id}
      data-cols={activeSpan.cols}
      data-rows={activeSpan.rows}
      data-lifecycle={lifecycle}
      data-pinned={pinned ? '' : undefined}
      data-resizing={liveSpan ? '' : undefined}
      data-drop-target={isDropTarget ? '' : undefined}
      onDragOver={(event) => {
        event.preventDefault();
        onDragOverId(artifact.artifact_id);
      }}
      onDragLeave={() => onDragOverId(null)}
      onDrop={(event) => {
        event.preventDefault();
        onDropOnId(artifact.artifact_id);
      }}
    >
      <ContextMenuRoot styleProfile="rhea">
        <ContextMenuTrigger styleProfile="rhea" className="widget-cm-trigger">
          <CardRoot styleProfile="rhea" size="sm" className="widget-tile">
            <CardHeader styleProfile="rhea" className="widget-head">
              <div
                className="widget-head-bar"
                draggable
                onDragStart={(event) => {
                  event.dataTransfer.effectAllowed = 'move';
                  event.dataTransfer.setData('text/plain', artifact.artifact_id);
                  onDragStartId(artifact.artifact_id);
                }}
                onDragEnd={onDragEnd}
              >
                <span className="widget-dot" data-lifecycle={lifecycle} aria-hidden="true" />
                <span className="widget-title" title={title}>{title}</span>
                <time className="widget-age" title={formatStamp(artifact.updated_at)}>
                  {formatCompactAge(artifact.updated_at)}
                </time>
                <div className="widget-actions" onMouseDown={(event) => event.stopPropagation()}>
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
                  <DropdownRoot styleProfile="rhea">
                    <DropdownTrigger
                      styleProfile="rhea"
                      className="widget-act widget-menu-trigger"
                      aria-label={t('widgets.menu')}
                    >
                      <Icon name="more" size={13} />
                    </DropdownTrigger>
                    <DropdownContent styleProfile="rhea" align="end" sideOffset={4} className="widget-menu-content">
                      {renderMenuItems(DropdownItem, true)}
                    </DropdownContent>
                  </DropdownRoot>
                </div>
              </div>
            </CardHeader>

            <CardContent styleProfile="rhea" className={`widget-body${hasLiveCanvas ? ' is-frame' : ''}`}>
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
            </CardContent>

            {!isSlim && !hasLiveCanvas && (
              <CardFooter styleProfile="rhea" className="widget-foot">
                <span>{metaParts.join(' · ')}</span>
              </CardFooter>
            )}
          </CardRoot>
        </ContextMenuTrigger>
        <ContextMenuContent styleProfile="rhea" className="widget-menu-content">
          {renderMenuItems(ContextMenuItem, false)}
        </ContextMenuContent>
      </ContextMenuRoot>

      <button
        type="button"
        className="widget-resize-grip"
        aria-label={t('widgets.resizeGrip')}
        title={t('widgets.resizeGrip')}
        onPointerDown={beginCornerResize}
      />
      {liveSpan && (
        <span className="widget-size-hud" aria-live="polite">
          {liveSpan.cols} × {liveSpan.rows}
        </span>
      )}
    </div>
  );
}

/**
 * Dynamic, freeform, template-free persistent Widgets board for Shenko Playground.
 * Backed by Proto-UI shadcn Card, Tabs, ContextMenu, Dropdown and Tree families:
 * adaptive 2/4/6-column dense spatial grid, arbitrary (cols × rows) proportions,
 * direct corner drag-to-resize, drag-and-drop reordering, right-click & dropdown
 * actions, plus Pinned / Recent / Archive / Index horizons.
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
  const [prefs, setPrefs] = React.useState<WidgetBoardPrefs>(() => readBoardPrefs());
  const [draggingId, setDraggingId] = React.useState<string | null>(null);
  const [dropTargetId, setDropTargetId] = React.useState<string | null>(null);
  const [resizingAny, setResizingAny] = React.useState(false);

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
    return {
      pinned: sortByOrder(pinnedList, prefs.order),
      recent: sortByOrder(recentList, prefs.order),
      live: liveList,
      archived: archivedList,
      newestMs: newest,
    };
  }, [artifacts, pinnedSet, prefs.order]);

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

  const updatePrefs = (updater: (prev: WidgetBoardPrefs) => WidgetBoardPrefs) => {
    setPrefs((prev) => {
      const next = updater(prev);
      saveBoardPrefs(next);
      return next;
    });
  };

  const togglePin = (artifactId: string) => {
    setWorkPinned(artifactId, !pinnedSet.has(artifactId));
    setPinnedIds(pinnedWorks());
  };

  const setSpan = (artifactId: string, span: WidgetSpan) => {
    updatePrefs((prev) => ({ ...prev, spans: { ...prev.spans, [artifactId]: clampSpan(span) } }));
  };

  const reorderIds = (sourceId: string, targetId: string) => {
    if (sourceId === targetId) return;
    updatePrefs((prev) => {
      const base = artifacts.map((item) => item.artifact_id);
      const current = [...prev.order.filter((id) => base.includes(id)), ...base.filter((id) => !prev.order.includes(id))];
      const from = current.indexOf(sourceId);
      const to = current.indexOf(targetId);
      if (from < 0 || to < 0) return prev;
      current.splice(from, 1);
      current.splice(to, 0, sourceId);
      return { ...prev, order: current };
    });
  };

  const moveToFront = (artifactId: string) => {
    updatePrefs((prev) => {
      const base = artifacts.map((item) => item.artifact_id);
      const rest = [...prev.order.filter((id) => base.includes(id)), ...base.filter((id) => !prev.order.includes(id))]
        .filter((id) => id !== artifactId);
      return { ...prev, order: [artifactId, ...rest] };
    });
  };

  const resetLayout = () => {
    updatePrefs(() => ({ spans: {}, order: [] }));
  };

  const hasCustomLayout = Object.keys(prefs.spans).length > 0 || prefs.order.length > 0;

  const openActionFor = (artifact: ArtifactRecord) => {
    if (isPage(artifact.mime) && onOpenWork) return () => onOpenWork(artifact.artifact_id);
    if (onInspectWork) return () => onInspectWork(artifact.artifact_id);
    return undefined;
  };

  const renderGrid = (items: readonly ArtifactRecord[]) => {
    const defaults = defaultSpansFor(items);
    return (
      <div className="widgets-grid" data-resizing={resizingAny ? '' : undefined}>
        {items.map((artifact, index) => (
          <WidgetSlot
            key={artifact.artifact_id}
            artifact={artifact}
            span={prefs.spans[artifact.artifact_id] ?? defaults[index]}
            pinned={pinnedSet.has(artifact.artifact_id)}
            demo={demo}
            theme={theme}
            preview={demo ? getDemoPreview?.(artifact.artifact_id, theme) : undefined}
            hasCustomLayout={hasCustomLayout}
            isDropTarget={Boolean(draggingId && draggingId !== artifact.artifact_id && dropTargetId === artifact.artifact_id)}
            onTogglePin={() => togglePin(artifact.artifact_id)}
            onSetSpan={(next) => setSpan(artifact.artifact_id, next)}
            onMoveFront={() => moveToFront(artifact.artifact_id)}
            onResetLayout={resetLayout}
            onOpen={openActionFor(artifact)}
            onDragStartId={(id) => setDraggingId(id)}
            onDragOverId={(id) => setDropTargetId(id)}
            onDropOnId={(targetId) => {
              if (draggingId) reorderIds(draggingId, targetId);
              setDraggingId(null);
              setDropTargetId(null);
            }}
            onDragEnd={() => {
              setDraggingId(null);
              setDropTargetId(null);
            }}
            onResizeState={setResizingAny}
          />
        ))}
      </div>
    );
  };

  const scopes: ReadonlyArray<{ id: WidgetScope; label: string; count: number }> = [
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
        <ScopeTabs
          value={scope}
          onChange={selectScope}
          label="Playground"
          items={scopes}
        />
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
