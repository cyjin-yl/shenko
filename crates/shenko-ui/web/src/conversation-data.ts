import { text as t } from './messages';
import * as React from 'react';
import type { ConversationRow } from './threads';

/// Channel conversations, re-read when a new step commits.
export function useConversations(live: boolean, step: number | null): { rows: ConversationRow[]; error?: string } {
  const [rows, setRows] = React.useState<ConversationRow[]>([]);
  const [error, setError] = React.useState<string>();
  React.useEffect(() => {
    if (!live) return;
    let current = true;
    fetch('/api/conversations', { cache: 'no-store' })
      .then((response) => {
        if (!response.ok) throw new Error(t('conversation.httpError', { status: response.status }));
        return response.json();
      })
      .then((data) => {
        if (!current) return;
        if (!data || data.available !== true || !Array.isArray(data.messages)) {
          throw new Error(typeof data?.note === 'string' ? data.note : t('conversation.unavailable'));
        }
        setRows(data.messages);
        setError(undefined);
      })
      .catch((failure) => { if (current) setError(failure instanceof Error ? failure.message : t('conversation.unavailable')); });
    return () => { current = false; };
  }, [live, step]);
  return { rows, error };
}
