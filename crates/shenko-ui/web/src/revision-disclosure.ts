/**
 * Taste revision disclosure — the *server's* projection of one unit's revision
 * chain (SPEC §51.1/§51.4). Producer:
 * `shenko_context_policy::taste::Disclosure::project_json()`, whose flat JSON is
 *
 *   family, selected, content_digest, written_at_ms, older_versions,
 *   latest_modified_at_ms, full_text
 *
 * `Selected` reports the default view with `full_text: null`; `Version(n)`
 * reports an explicitly requested revision and sets `older_versions` /
 * `latest_modified_at_ms` to null while `full_text` carries
 * `{revision, written_at_ms, text}`. Any field may be null because the producer
 * only reports what it actually knows (an unregistered revision has no digest,
 * an unknown selected revision has no number, unresolved bytes have no text).
 *
 * This reader never counts revisions, never derives a chain and never re-reads
 * old content: an unreadable projection is treated as absent instead of being
 * reconstructed.
 */
export interface RevisionFullText {
  revision: number;
  written_at_ms: number | null;
  text: string | null;
}

export interface RevisionDisclosure {
  family: string;
  /** Revision number the default projection currently reads. */
  selected: number | null;
  content_digest: string | null;
  written_at_ms: number | null;
  /** Count of retained older versions, as reported by the server. */
  older_versions: number | null;
  latest_modified_at_ms: number | null;
  /** Present only when an explicit revision was requested. */
  full_text: RevisionFullText | null;
  /** The server sent a `full_text` record this UI cannot read. */
  full_text_unreadable: boolean;
}

function numberOrNull(value: unknown): number | null {
  return typeof value === 'number' && Number.isFinite(value) ? value : null;
}

function nonEmptyTextOrNull(value: unknown): string | null {
  return typeof value === 'string' && value.length > 0 ? value : null;
}

export function readRevisionDisclosure(value: unknown): RevisionDisclosure | null {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  const family = nonEmptyTextOrNull(record.family);
  if (family === null) return null;
  const raw = record.full_text;
  let fullText: RevisionFullText | null = null;
  let fullTextUnreadable = false;
  if (raw !== undefined && raw !== null) {
    const entry: Record<string, unknown> | null = typeof raw === 'object' ? raw as Record<string, unknown> : null;
    const revision = entry ? numberOrNull(entry.revision) : null;
    if (entry && revision !== null) {
      fullText = {
        revision,
        written_at_ms: numberOrNull(entry.written_at_ms),
        text: typeof entry.text === 'string' ? entry.text : null,
      };
    } else fullTextUnreadable = true;
  }
  const disclosure: RevisionDisclosure = {
    family,
    selected: numberOrNull(record.selected),
    content_digest: nonEmptyTextOrNull(record.content_digest),
    written_at_ms: numberOrNull(record.written_at_ms),
    older_versions: numberOrNull(record.older_versions),
    latest_modified_at_ms: numberOrNull(record.latest_modified_at_ms),
    full_text: fullText,
    full_text_unreadable: fullTextUnreadable,
  };
  const reportsSomething = disclosure.selected !== null || disclosure.content_digest !== null ||
    disclosure.written_at_ms !== null || disclosure.older_versions !== null ||
    disclosure.latest_modified_at_ms !== null || disclosure.full_text !== null ||
    disclosure.full_text_unreadable;
  return reportsSomething ? disclosure : null;
}

/**
 * Store-level revision summary (`shenko_types::cognition::ContentRevisionDisclosure`:
 * `latest_digest`, `old_version_count`, `latest_modified_at_ms`). A different
 * producer from the installed taste disclosure: no selection and no full text,
 * and the count is the server's, not a client tally.
 */
export interface ContentRevisionDisclosure {
  latest_digest: string;
  old_version_count: number;
  latest_modified_at_ms: number;
}

export function readContentRevisionDisclosure(value: unknown): ContentRevisionDisclosure | null {
  if (!value || typeof value !== 'object') return null;
  const record = value as Record<string, unknown>;
  const latestDigest = nonEmptyTextOrNull(record.latest_digest);
  const oldVersionCount = numberOrNull(record.old_version_count);
  const latestModified = numberOrNull(record.latest_modified_at_ms);
  if (latestDigest === null || oldVersionCount === null || latestModified === null) return null;
  return { latest_digest: latestDigest, old_version_count: oldVersionCount, latest_modified_at_ms: latestModified };
}
