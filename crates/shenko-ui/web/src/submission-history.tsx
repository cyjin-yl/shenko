import { text as t, resolveMessage } from './messages';
import * as React from 'react';
import type { UiNode } from './projection';
import { Button, type Emit } from './ui-kit';

export function SubmissionHistory({ node, emit, beforeLoad }: { node: UiNode; emit: Emit; beforeLoad?: () => void }) {
  const history = node.props.submission_history;
  if (!history || !history.loading && !history.error && !history.has_older) return null;
  return <div className="im-system">
    {history.loading && <span role="status">{t('history.loading')}</span>}
    {history.error && <span role="status">{t('history.error', { detail: resolveMessage(history.error) })} </span>}
    {!history.loading && (history.has_older || history.error) && <span data-load-submissions="true"><Button
      variant="ghost" size="sm"
      onClick={() => { beforeLoad?.(); emit('timeline', 'submissions.load_older'); }}
    >{history.error ? t('history.retry') : t('history.older')}</Button></span>}
  </div>;
}
