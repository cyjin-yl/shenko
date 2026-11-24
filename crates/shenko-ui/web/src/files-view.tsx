import { text as t } from './messages';
import * as React from 'react';
import { Icon } from './icons';
import { formatBytes } from './composer';
import { Button, ScrollBox, formatTime } from './ui-kit';

/**
 * The Finder-like file view: a sidebar of every file system the subject can
 * reach — its WASM home first, then the other mounts its home declares, then
 * other nodes — and a list of the open folder with a path bar. Roots and their
 * protocols are server facts (validated startup mounts, the host's mount table, the
 * node's sync peers); this view only draws them. Browsing is read-only.
 */
export interface FileRoot {
  id: string;
  label: string;
  kind: 'home' | 'mount' | 'node';
  protocol: string;
  guest_path: string;
  mode: string | null;
  writable: boolean;
  browsable: boolean;
  note: string | null;
}

export interface FileEntry {
  /** Opaque native filename component, never reconstructed from the label. */
  id: string;
  name: string;
  kind: 'dir' | 'file' | 'symlink' | 'other';
  bytes: number;
  modified: string | null;
}

export interface FileRootsAnswer {
  available: boolean;
  roots: FileRoot[];
  nodes_known: boolean;
  error?: string;
}

export interface FileListAnswer {
  available: boolean;
  entries: FileEntry[];
  truncated: boolean;
  error?: string;
}

export interface FileSource {
  roots(): Promise<FileRootsAnswer>;
  /** Slash-separated native component IDs; the empty path names the root. */
  list(root: string, path: string): Promise<FileListAnswer>;
}

export interface RecentFileHint {
  name: string;
  bytes: number;
  mime: string;
}

export const liveFiles: FileSource = {
  async roots() {
    const response = await fetch('/api/files/roots', { cache: 'no-store' });
    const body = await response.json() as FileRootsAnswer;
    if (!response.ok) throw new Error(body.error ?? t('files.unavailable'));
    if (typeof body.available !== 'boolean' || !Array.isArray(body.roots) || typeof body.nodes_known !== 'boolean') {
      throw new Error(t('files.invalidResponse'));
    }
    return body;
  },
  async list(root, path) {
    const response = await fetch(`/api/files/list?root=${encodeURIComponent(root)}&path=${encodeURIComponent(path)}`, { cache: 'no-store' });
    const body = await response.json() as FileListAnswer;
    if (!response.ok) throw new Error(body.error ?? t('files.unavailable'));
    if (typeof body.available !== 'boolean' || !Array.isArray(body.entries) || typeof body.truncated !== 'boolean') {
      throw new Error(t('files.invalidResponse'));
    }
    return body;
  },
};

/// The icon for a root: the sandbox for the home, a drive for a local mount,
/// a network share for sshfs/NFS/SMB, a shared folder, a node.
function rootIcon(root: FileRoot): string {
  if (root.kind === 'home') return 'sandbox';
  if (root.kind === 'node') return 'node';
  if (root.protocol === 'shared_folder') return 'share';
  if (['sshfs', 'nfs', 'smb'].includes(root.protocol)) return 'network';
  return 'drive';
}

function protocolLabel(root: FileRoot): string {
  switch (root.protocol) {
    case 'wasm_home': return t('files.protocol.wasm');
    case 'local': return t('files.protocol.local');
    case 'sshfs': return 'sshfs';
    case 'nfs': return 'NFS';
    case 'smb': return 'SMB';
    case 'shared_folder': return t('files.protocol.shared');
    case 'ssh': return 'SSH';
    default: return root.protocol;
  }
}

function shortProtocol(root: FileRoot): string {
  switch (root.protocol) {
    case 'wasm_home': return 'WASM';
    case 'local': return '';
    case 'sshfs': return 'SSHFS';
    case 'nfs': return 'NFS';
    case 'smb': return 'SMB';
    case 'shared_folder': return '';
    case 'ssh': return 'SSH';
    default: return root.protocol.toUpperCase();
  }
}

function shortMode(mode: string | null): string {
  if (mode === 'read_only') return t('files.badge.ro');
  if (mode === 'copy_on_write') return t('files.badge.cow');
  if (mode === 'read_write') return t('files.badge.rw');
  return '';
}

function rootBadge(root: FileRoot): string {
  if (!root.browsable) return [shortProtocol(root), t('files.badge.offline')].filter(Boolean).join(' · ');
  return [shortProtocol(root), shortMode(root.mode)].filter(Boolean).join(' · ');
}

function modeLabel(mode: string | null): string {
  if (mode === 'read_only') return t('files.mode.ro');
  if (mode === 'copy_on_write') return t('files.mode.cow');
  if (mode === 'read_write') return t('files.mode.rw');
  return '';
}

function kindLabel(entry: FileEntry): string {
  if (entry.kind === 'dir') return t('files.kind.folder');
  if (entry.kind === 'symlink') return t('files.kind.link');
  if (entry.kind === 'other') return t('files.kind.other');
  const dot = entry.name.lastIndexOf('.');
  return dot > 0 ? t('files.kind.typed', { ext: entry.name.slice(dot + 1).toUpperCase() }) : t('files.kind.file');
}

function when(value: string | null): string {
  if (!value) return '—';
  const date = new Date(value);
  if (date.toDateString() === new Date().toDateString()) return formatTime(value);
  return date.toLocaleDateString(undefined, { year: 'numeric', month: 'short', day: 'numeric' });
}

interface VisitedPlace {
  key: string;
  rootId: string;
  rootLabel: string;
  path: Array<{ id: string; name: string }>;
}

export function FilesView({ source, recentHints = [] }: {
  source: FileSource;
  recentHints?: readonly RecentFileHint[];
}) {
  const [roots, setRoots] = React.useState<FileRoot[] | null>(null);
  const [rootsAvailable, setRootsAvailable] = React.useState(false);
  const [rootsError, setRootsError] = React.useState<string | null>(null);
  const [nodesKnown, setNodesKnown] = React.useState(false);
  const [rootId, setRootId] = React.useState<string | null>(null);
  const [path, setPath] = React.useState<Array<{ id: string; name: string }>>([]);
  const [entries, setEntries] = React.useState<FileEntry[] | null>(null);
  const [error, setError] = React.useState<string | null>(null);
  const [truncated, setTruncated] = React.useState(false);
  const [selected, setSelected] = React.useState<string | null>(null);
  const [filter, setFilter] = React.useState('');
  const [collapsedGroups, setCollapsedGroups] = React.useState<Record<string, boolean>>({});
  const [visited, setVisited] = React.useState<VisitedPlace[]>([]);
  const searchRef = React.useRef<HTMLInputElement>(null);

  React.useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if ((event.metaKey || event.ctrlKey) && !event.altKey && !event.shiftKey && event.key.toLowerCase() === 'f') {
        if (!searchRef.current) return;
        event.preventDefault();
        searchRef.current.focus();
        searchRef.current.select();
      }
    };
    window.addEventListener('keydown', onKey);
    return () => window.removeEventListener('keydown', onKey);
  }, []);

  React.useEffect(() => {
    let live = true;
    source.roots().then((answer) => {
      if (!live) return;
      setRootsAvailable(answer.available);
      setRootsError(answer.error !== undefined || !answer.available ? answer.error || t('files.unavailable') : null);
      setRoots(answer.roots);
      setNodesKnown(answer.nodes_known);
      setRootId((current) => current ?? answer.roots.find((root) => root.browsable)?.id ?? null);
    }).catch((reason) => {
      if (!live) return;
      setRoots([]);
      setRootsAvailable(false);
      setRootsError(String(reason?.message ?? reason) || t('files.unavailable'));
    });
    return () => { live = false; };
  }, [source]);

  const root = roots?.find((item) => item.id === rootId) ?? null;
  const pathId = path.map((segment) => segment.id).join('/');
  React.useEffect(() => {
    if (!root || !root.browsable || !rootsAvailable) { setEntries(null); setTruncated(false); setError(null); return; }
    let live = true;
    setEntries(null);
    setTruncated(false);
    setError(null);
    source.list(root.id, pathId).then((answer) => {
      if (!live) return;
      const failed = !answer.available || answer.error !== undefined;
      setEntries(failed ? null : answer.entries);
      setTruncated(!failed && answer.truncated);
      setError(failed ? answer.error || t('files.unavailable') : null);
    }).catch((reason) => live && setError(String(reason?.message ?? reason) || t('files.unavailable')));
    return () => { live = false; };
  }, [source, root?.id, root?.browsable, rootsAvailable, pathId]);

  const open = (id: string, nextPath: Array<{ id: string; name: string }> = []) => {
    setRootId(id);
    setPath(nextPath);
    setSelected(null);
  };
  const enterFolder = (entry: FileEntry) => {
    if (entry.kind !== 'dir' || !root) return;
    const nextPath = [...path, { id: entry.id, name: entry.name }];
    setPath(nextPath);
    setSelected(null);
    const key = `${root.id}:${nextPath.map((s) => s.id).join('/')}`;
    setVisited((previous) => [
      { key, rootId: root.id, rootLabel: root.label || root.guest_path, path: nextPath },
      ...previous.filter((item) => item.key !== key),
    ].slice(0, 4));
  };
  const toggleGroup = (key: string) => {
    setCollapsedGroups((previous) => ({ ...previous, [key]: !previous[key] }));
  };

  const query = filter.trim().toLowerCase();
  const matchesRoot = (item: FileRoot) => !query
    || (item.label || '').toLowerCase().includes(query)
    || item.guest_path.toLowerCase().includes(query)
    || item.protocol.toLowerCase().includes(query)
    || rootBadge(item).toLowerCase().includes(query);

  const allGroups: Array<{ key: 'home' | 'mount' | 'node'; label: string; items: FileRoot[] }> = [
    { key: 'home', label: t('files.group.home'), items: roots?.filter((item) => item.kind === 'home') ?? [] },
    { key: 'mount', label: t('files.group.mounts'), items: roots?.filter((item) => item.kind === 'mount') ?? [] },
    { key: 'node', label: t('files.group.nodes'), items: roots?.filter((item) => item.kind === 'node') ?? [] },
  ];
  const visibleRoots = (roots ?? []).filter(matchesRoot);
  const visibleEntries = entries === null ? null
    : !query ? entries : entries.filter((entry) => entry.name.toLowerCase().includes(query) || kindLabel(entry).toLowerCase().includes(query));
  const selectedEntry = visibleEntries?.find((entry) => entry.id === selected) ?? null;
  const visibleVisited = visited.filter((place) => !query
    || place.path.some((seg) => seg.name.toLowerCase().includes(query))
    || place.rootLabel.toLowerCase().includes(query));
  const visibleHints = recentHints.filter((hint) => !query || hint.name.toLowerCase().includes(query));
  const hasRecent = visibleVisited.length > 0 || visibleHints.length > 0;
  const homeRoot = roots?.find((item) => item.kind === 'home' && item.browsable) ?? null;

  return (
    <div className="files">
      <aside className="files-side" aria-label={t('files.locations')}>
        <div className="files-search-wrap">
          <Icon name="search" size={13} />
          <input
            ref={searchRef}
            type="search"
            className="files-search"
            value={filter}
            onChange={(event) => setFilter(event.currentTarget.value)}
            placeholder={t('files.searchPlaceholder')}
            aria-label={t('files.searchPlaceholder')}
          />
          {filter ? (
            <button
              type="button"
              className="files-search-clear"
              onClick={() => setFilter('')}
              aria-label={t('files.searchClear')}
            >
              <Icon name="close" size={12} />
            </button>
          ) : (
            <kbd className="files-search-kbd" aria-hidden="true">⌘F</kbd>
          )}
        </div>
        <ScrollBox className="files-side-scroll" viewportClassName="files-side-viewport">
          <nav className="files-side-nav" aria-label={t('files.locations')}>
            {hasRecent && (
              <section className="files-group">
                <button
                  type="button"
                  className="files-group-toggle"
                  aria-expanded={!collapsedGroups.recent}
                  onClick={() => toggleGroup('recent')}
                >
                  <Icon name="chevron" size={11} />
                  <span className="files-group-title">{t('files.recent')}</span>
                  <span className="files-group-count">{visibleVisited.length + visibleHints.length}</span>
                </button>
                {!collapsedGroups.recent && (
                  <div className="files-group-list">
                    {visibleVisited.map((place) => {
                      const leaf = place.path[place.path.length - 1]?.name ?? place.rootLabel;
                      const active = rootId === place.rootId && pathId === place.path.map((s) => s.id).join('/');
                      return (
                        <button
                          key={place.key}
                          type="button"
                          className="files-root is-recent"
                          data-active={active ? '' : undefined}
                          onClick={() => open(place.rootId, place.path)}
                          title={`${place.rootLabel}/${place.path.map((s) => s.name).join('/')}`}
                        >
                          <Icon name="folder" size={15} />
                          <span className="files-root-label">{leaf}</span>
                          <span className="files-root-badge">{place.rootLabel}</span>
                        </button>
                      );
                    })}
                    {visibleHints.map((hint, index) => (
                      <button
                        key={`${hint.name}-${index}`}
                        type="button"
                        className="files-root is-recent"
                        onClick={() => {
                          if (homeRoot) open(homeRoot.id);
                          setFilter(hint.name);
                        }}
                        title={`${hint.name} · ${formatBytes(hint.bytes)}`}
                      >
                        <Icon name="file" size={15} />
                        <span className="files-root-label">{hint.name}</span>
                        <span className="files-root-badge">{formatBytes(hint.bytes)}</span>
                      </button>
                    ))}
                  </div>
                )}
              </section>
            )}
            {allGroups.map(({ key, label, items }) => {
              const filteredItems = items.filter(matchesRoot);
              const showEmptyNote = items.length === 0 && (
                roots === null || !rootsAvailable || (key === 'node' && !nodesKnown)
              );
              if (query && filteredItems.length === 0) return null;
              if (!showEmptyNote && items.length === 0) return null;
              const isCollapsed = collapsedGroups[key] === true;
              return (
                <section key={key} className="files-group">
                  <button
                    type="button"
                    className="files-group-toggle"
                    aria-expanded={!isCollapsed}
                    onClick={() => toggleGroup(key)}
                  >
                    <Icon name="chevron" size={11} />
                    <span className="files-group-title">{label}</span>
                    {items.length > 0 && <span className="files-group-count">{filteredItems.length}</span>}
                  </button>
                  {!isCollapsed && (
                    <div className="files-group-list">
                      {showEmptyNote && (
                        <p className="files-group-empty">
                          {roots === null ? t('common.loading') : !rootsAvailable ? t('files.unavailable') : t('files.nodesUnknown')}
                        </p>
                      )}
                      {filteredItems.map((item) => {
                        const badge = rootBadge(item);
                        return (
                          <button
                            key={item.id}
                            type="button"
                            className="files-root"
                            data-active={item.id === rootId ? '' : undefined}
                            data-protocol={item.protocol}
                            data-browsable={item.browsable ? 'true' : 'false'}
                            onClick={() => open(item.id)}
                            title={[item.label || item.guest_path, protocolLabel(item), modeLabel(item.mode), item.guest_path, item.note].filter(Boolean).join(' · ')}
                          >
                            <span className={`files-status-dot${item.browsable ? ' is-live' : ' is-offline'}`} aria-hidden="true" />
                            <Icon name={rootIcon(item)} size={15} />
                            <span className="files-root-label">{item.label || item.guest_path}</span>
                            {badge && (
                              <span
                                className="files-root-badge"
                                data-mode={item.mode ?? (item.browsable ? 'default' : 'offline')}
                              >
                                {badge}
                              </span>
                            )}
                          </button>
                        );
                      })}
                    </div>
                  )}
                </section>
              );
            })}
          </nav>
        </ScrollBox>
      </aside>

      <div className="files-mobile-bar" aria-label={t('files.locations')}>
        <ScrollBox orientation="horizontal" className="files-mobile-scroll" viewportClassName="files-mobile-viewport">
          <div className="files-mobile-pills">
            {visibleRoots.map((item) => (
              <button
                key={item.id}
                type="button"
                className="files-mobile-pill"
                data-active={item.id === rootId ? '' : undefined}
                data-browsable={item.browsable ? 'true' : 'false'}
                onClick={() => open(item.id)}
              >
                <span className={`files-status-dot${item.browsable ? ' is-live' : ' is-offline'}`} aria-hidden="true" />
                <Icon name={rootIcon(item)} size={14} />
                <span>{item.label || item.guest_path}</span>
                {shortMode(item.mode) && <span className="files-pill-mode">{shortMode(item.mode)}</span>}
              </button>
            ))}
          </div>
        </ScrollBox>
      </div>

      <section className="files-main" aria-label={root?.label ?? t('files.title')}>
        <header className="files-bar">
          <ScrollBox orientation="horizontal" className="files-path-scroll" viewportClassName="files-path-viewport">
            <nav className="files-path" aria-label={t('files.path')}>
              <button type="button" className="files-crumb" onClick={() => { setPath([]); setSelected(null); }} disabled={!root}>
                {root ? <Icon name={rootIcon(root)} size={14} /> : null}
                <span>{root?.label ?? t('files.title')}</span>
              </button>
              {path.map((segment, index) => (
                <React.Fragment key={path.slice(0, index + 1).map((part) => part.id).join('/')}>
                  <Icon name="chevron" size={11} />
                  <button
                    type="button"
                    className="files-crumb"
                    onClick={() => { setPath(path.slice(0, index + 1)); setSelected(null); }}
                  >
                    {segment.name}
                  </button>
                </React.Fragment>
              ))}
            </nav>
          </ScrollBox>
          {root && (
            <span className="files-facts">
              <span className="files-fact-chip">{protocolLabel(root)}</span>
              {modeLabel(root.mode) && <span className="files-fact-chip">{modeLabel(root.mode)}</span>}
              <code className="files-fact-path">{root.guest_path}</code>
            </span>
          )}
        </header>
        <div className="files-banners">
          {root?.note && <p className="files-note">{root.note}</p>}
          {rootsError && <p className="files-note is-error">{rootsError}</p>}
          {error && <p className="files-note is-error">{error}</p>}
          {truncated && <p className="files-note" role="status">{t('files.truncated')}</p>}
          {rootsAvailable && !rootsError && roots?.length === 0 && <p className="files-empty">{t('files.noRoots')}</p>}
        </div>
        {root?.browsable && rootsAvailable && (
          <>
            <ScrollBox className="files-table-scroll" viewportClassName="files-table-viewport">
              <div className="files-table" role="grid" aria-label={t('files.contents')}>
                <div className="files-row files-head" role="row">
                  <span role="columnheader">{t('files.col.name')}</span>
                  <span role="columnheader">{t('files.col.modified')}</span>
                  <span role="columnheader" className="is-num">{t('files.col.size')}</span>
                  <span role="columnheader">{t('files.col.kind')}</span>
                </div>
                {entries === null && !error && <p className="files-empty">{t('common.loading')}</p>}
                {entries?.length === 0 && !error && !truncated && <p className="files-empty">{t('files.emptyFolder')}</p>}
                {entries && entries.length > 0 && visibleEntries?.length === 0 && (
                  <p className="files-empty">{t('files.noMatches')}</p>
                )}
                {visibleEntries?.map((entry) => (
                  <div
                    key={entry.id}
                    role="row"
                    tabIndex={0}
                    className="files-row"
                    data-kind={entry.kind}
                    data-selected={selected === entry.id ? '' : undefined}
                    onClick={() => setSelected(entry.id)}
                    onDoubleClick={() => enterFolder(entry)}
                    onKeyDown={(event) => {
                      if (event.key === 'Enter') enterFolder(entry);
                    }}
                  >
                    <span role="gridcell" className="files-name" aria-label={entry.name}>
                      <span className="files-icon" aria-hidden="true">
                        <Icon name={entry.kind === 'dir' ? 'folder' : 'file'} size={15} />
                      </span>
                      <span className="files-name-text">{entry.name}</span>
                    </span>
                    <span role="gridcell" className="files-dim">{when(entry.modified)}</span>
                    <span role="gridcell" className="files-dim is-num">{entry.kind === 'file' ? formatBytes(entry.bytes) : '—'}</span>
                    <span role="gridcell" className="files-dim">{kindLabel(entry)}</span>
                  </div>
                ))}
              </div>
            </ScrollBox>
            <footer className="files-statusbar">
              <span className="files-status-left">
                {entries !== null && (
                  <span className="files-status-count">
                    {query
                      ? t('files.filteredCount', { shown: visibleEntries?.length ?? 0, total: entries.length })
                      : t('files.itemsCount', { count: entries.length })}
                  </span>
                )}
                {selectedEntry && (
                  <span className="files-status-selected">
                    <strong>{selectedEntry.name}</strong>
                    <span>{selectedEntry.kind === 'file' ? formatBytes(selectedEntry.bytes) : kindLabel(selectedEntry)}</span>
                    {selectedEntry.kind === 'dir' && (
                      <Button variant="ghost" size="sm" className="files-open-dir" onClick={() => enterFolder(selectedEntry)}>
                        {t('files.openFolder')}
                      </Button>
                    )}
                  </span>
                )}
              </span>
              <span className="files-status-right">
                <Icon name="lock" size={12} />
                <span>{t('files.statusReadonly')}</span>
              </span>
            </footer>
          </>
        )}
      </section>
    </div>
  );
}
