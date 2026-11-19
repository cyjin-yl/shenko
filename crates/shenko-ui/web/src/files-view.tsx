import { text as t } from './messages';
import * as React from 'react';
import { Icon } from './icons';
import { formatBytes } from './composer';
import { formatTime } from './ui-kit';

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

export function FilesView({ source }: { source: FileSource }) {
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

  const open = (id: string) => { setRootId(id); setPath([]); setSelected(null); };
  const groups: Array<[string, FileRoot[]]> = [
    [t('files.group.home'), roots?.filter((item) => item.kind === 'home') ?? []],
    [t('files.group.mounts'), roots?.filter((item) => item.kind === 'mount') ?? []],
    [t('files.group.nodes'), roots?.filter((item) => item.kind === 'node') ?? []],
  ];

  return (
    <div className="files">
      <nav className="files-side" aria-label={t('files.locations')}>
        {groups.map(([label, items]) => (
          <section key={label} className="files-group">
            <h3 className="files-group-title">{label}</h3>
            {items.length === 0 && <p className="files-group-empty">
              {roots === null ? t('common.loading') : !rootsAvailable ? t('files.unavailable')
                : label === t('files.group.nodes') && !nodesKnown ? t('files.nodesUnknown') : t('files.none')}
            </p>}
            {items.map((item) => (
              <button
                key={item.id}
                type="button"
                className="files-root"
                data-active={item.id === rootId ? '' : undefined}
                data-protocol={item.protocol}
                onClick={() => open(item.id)}
                title={item.note ?? item.guest_path}
              >
                <Icon name={rootIcon(item)} size={17} />
                <span className="files-root-label">{item.label || item.guest_path}</span>
              </button>
            ))}
          </section>
        ))}
      </nav>
      <section className="files-main" aria-label={root?.label ?? t('files.title')}>
        <header className="files-bar">
          <nav className="files-path" aria-label={t('files.path')}>
            <button type="button" className="files-crumb" onClick={() => setPath([])} disabled={!root}>
              {root ? <Icon name={rootIcon(root)} size={15} /> : null}{root?.label ?? t('files.title')}
            </button>
            {path.map((segment, index) => (
              <React.Fragment key={path.slice(0, index + 1).map((part) => part.id).join('/')}>
                <Icon name="chevron" size={12} />
                <button type="button" className="files-crumb" onClick={() => setPath(path.slice(0, index + 1))}>{segment.name}</button>
              </React.Fragment>
            ))}
          </nav>
          {root && (
            <span className="files-facts">
              {[protocolLabel(root), modeLabel(root.mode), root.guest_path].filter(Boolean).join(' · ')}
            </span>
          )}
        </header>
        <div>{root?.note && <p className="files-note">{root.note}</p>}</div>
        <div>
          {rootsError && <p className="files-note is-error">{rootsError}</p>}
          {error && <p className="files-note is-error">{error}</p>}
          {truncated && <p className="files-note" role="status">{t('files.truncated')}</p>}
          {rootsAvailable && !rootsError && roots?.length === 0 && <p className="files-empty">{t('files.noRoots')}</p>}
        </div>
        {root?.browsable && rootsAvailable && (
          <div className="files-table" role="grid" aria-label={t('files.contents')}>
            <div className="files-row files-head" role="row">
              <span role="columnheader">{t('files.col.name')}</span>
              <span role="columnheader">{t('files.col.modified')}</span>
              <span role="columnheader" className="is-num">{t('files.col.size')}</span>
              <span role="columnheader">{t('files.col.kind')}</span>
            </div>
            {entries === null && !error && <p className="files-empty">{t('common.loading')}</p>}
            {entries?.length === 0 && !error && !truncated && <p className="files-empty">{t('files.emptyFolder')}</p>}
            {entries?.map((entry) => (
              <div
                key={entry.id}
                role="row"
                tabIndex={0}
                className="files-row"
                data-kind={entry.kind}
                data-selected={selected === entry.id ? '' : undefined}
                onClick={() => setSelected(entry.id)}
                onDoubleClick={() => { if (entry.kind === 'dir') { setPath([...path, { id: entry.id, name: entry.name }]); setSelected(null); } }}
                onKeyDown={(event) => {
                  if (event.key === 'Enter' && entry.kind === 'dir') { setPath([...path, { id: entry.id, name: entry.name }]); setSelected(null); }
                }}
              >
                <span role="gridcell" className="files-name">
                  <span className="files-icon"><Icon name={entry.kind === 'dir' ? 'folder' : 'file'} size={16} /></span>
                  {entry.name}
                </span>
                <span role="gridcell" className="files-dim">{when(entry.modified)}</span>
                <span role="gridcell" className="files-dim is-num">{entry.kind === 'file' ? formatBytes(entry.bytes) : '—'}</span>
                <span role="gridcell" className="files-dim">{kindLabel(entry)}</span>
              </div>
            ))}
          </div>
        )}
      </section>
    </div>
  );
}
