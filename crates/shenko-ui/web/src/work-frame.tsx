import { text as t } from './messages';
import * as React from 'react';
import { Button } from './ui-kit';
import { pinnedWorks, setWorkPinned } from './slots';

/**
 * A work's published page, live beside the stream. Works are trusted
 * extensions by default: the subject wrote,
 * tested and can roll them back, so the page shares the shell's origin. The
 * safety rope is the named revision and the shell's safe mode (`?safe=1`),
 * which opens no work page. Its revision keys the frame, so a new revision is
 * a fresh page.
 */
export function WorkFrame({ artifactId, title, revision, activeManifestDigest }: { artifactId: string; title: string; revision?: number; activeManifestDigest?: string }) {
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
      .catch(() => { if (live) setError(t('work.adoptionUnavailable')); });
    return () => { live = false; };
  }, [artifactId, revision, activeManifestDigest]);
  if (SAFE_MODE) {
    return <div className="work-frame"><p className="muted safe-note">{t('work.safeMode')}</p></div>;
  }
  const src = draft ? `/playground/${artifactId}` : `/playground/${artifactId}/adopted`;
  return (
    <div className="work-frame">
      <div className="work-frame-bar">
        <span className="muted">
          {draft ? t('work.draftRevision', { revision: revision ?? '—' }) : adopted.state === 'ready' ? t('work.adoptedRevision', { revision: adopted.revision ?? '—' }) : adopted.state === 'none' ? t('work.notAdopted') : ''}
        </span>
        {draft && adopted.state === 'ready' && <Button variant="ghost" size="sm" onClick={() => setDraft(false)}>{t('work.returnAdopted')}</Button>}
        {!draft && adopted.state === 'ready' && revision !== undefined && String(revision) !== adopted.revision && (
          <Button variant="ghost" size="sm" onClick={() => setDraft(true)}>{t('work.previewDraft', { revision: revision ?? '—' })}</Button>
        )}
        <Button variant="ghost" size="sm" onClick={() => { setWorkPinned(artifactId, !pinned); rerender(); }}>
          {pinned ? t('work.unpin') : t('work.pin')}
        </Button>
        <a className="work-frame-link" href={src} target="_blank" rel="noreferrer noopener">{t('work.newWindow')}</a>
      </div>
      {error && <p className="muted" role="status">{error}</p>}
      {!draft && adopted.state === 'none'
        ? <div className="work-frame-empty">
          <p className="muted">{t('work.noAdoptedPage')}</p>
          <Button variant="outline" size="sm" onClick={() => setDraft(true)}>{t('work.previewUnadopted', { revision: revision ?? '—' })}</Button>
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
/// trusted extension misbehaves.
const SAFE_MODE = new URLSearchParams(location.search).get('safe') === '1';
