// Shenko's query composition, written against the audited Obelisk source CLI's
// helpers. No upstream implementation is bundled. Obelisk remains an external
// AGPL-3.0-only program: https://github.com/tommy0103/obelisk
// request and binding are JSON data supplied by the host, never transcript code.
const scope = (input) => ({
  source: input.provider ?? undefined,
  sessionId: input.session_id ?? undefined,
  projectPath: input.project_path ?? undefined,
  after: input.after ?? undefined,
  before: input.before ?? undefined,
  limit: input.limit,
  includeInactive: input.include_inactive === true,
});
const visibility = request.include_inactive
  ? "COALESCE(m.visibility,'visible') IN ('visible','inactive')"
  : "COALESCE(m.visibility,'visible')='visible'";
let data;
let manifest_rows = [];
let manifest_next_path = null;
switch (request.op) {
  case 'sessions':
    data = sessions(scope(request));
    break;
  case 'messages':
    data = messages({
      sessionId: request.session_id, limit: request.limit,
      cursor: request.cursor ?? undefined,
      includeInactive: request.include_inactive,
      includeSession: true,
    });
    break;
  case 'raw_page': {
    const page = raw(request.message_uuid, {
      // One extra UTF-16 unit lets a page end after an entire surrogate pair.
      offset: request.offset_utf16, limit: request.limit_utf16 + 1,
      includeInactive: request.include_inactive,
    });
    // null is deliberately not an empty/successful source read. The source may
    // be gone, changed, hidden, or outside this provider's raw support.
    if (page === null) {
      data = {status: 'source_unavailable', message_uuid: request.message_uuid,
        reason: 'upstream_raw_lookup_unavailable', offset_unit: 'utf16_code_units'};
    } else if (page.offset > page.totalLength) {
      data = {status: 'invalid_offset', reason: 'offset_after_source_end',
        message_uuid: request.message_uuid, offset_unit: 'utf16_code_units'};
    } else if (page.text.length && page.text.charCodeAt(0) >= 0xdc00 && page.text.charCodeAt(0) <= 0xdfff) {
      data = {status: 'invalid_offset', reason: 'offset_splits_a_surrogate_pair',
        message_uuid: request.message_uuid, offset_unit: 'utf16_code_units'};
    } else {
      let count = Math.min(request.limit_utf16, page.text.length);
      const last = count ? page.text.charCodeAt(count - 1) : 0;
      if (last >= 0xd800 && last <= 0xdbff && count < page.text.length) count++;
      const text = page.text.slice(0, count);
      const hasMore = page.offset + count < page.totalLength;
      data = {status: 'available', message_uuid: request.message_uuid,
        offset_unit: 'utf16_code_units', ...page, text, limit: count, hasMore,
        next_offset_utf16: hasMore ? page.offset + count : null,
        snapshot_consistent_across_pages: false};
    }
    break;
  }
  case 'search': {
    const hits = search(request.query, {...scope(request), contextLimit: 0, snippetTokens: 64});
    if (hits.length || !request.literal_fallback) {
      data = {mode: 'fts5', hits, searched_projection: 'obelisk_index',
        full_original_coverage: false};
      break;
    }
    // A literal fallback repairs unsegmented CJK substring matching inside the
    // indexed projection. It does not recover source tails truncated at ingest.
    const predicates = [visibility, 'COALESCE(m.is_meta,0)=0', 'instr(m.text,?)>0'];
    const args = [request.query];
    for (const [field, value] of [
      ['m.source', request.provider], ['m.session_id', request.session_id],
      ['s.project_path', request.project_path],
    ]) {
      if (value !== undefined && value !== null) { predicates.push(`${field}=?`); args.push(value); }
    }
    if (request.after) { predicates.push('m.timestamp>?'); args.push(request.after); }
    if (request.before) { predicates.push('m.timestamp<?'); args.push(request.before); }
    args.push(request.limit);
    data = {mode: 'literal_index_fallback', searched_projection: 'obelisk_index',
      full_original_coverage: false,
      hits: sql(`SELECT m.uuid,m.session_id,m.role,m.timestamp,m.parent_uuid,m.agent_id,
        m.content_type,m.visibility,m.source,
        substr(m.text,max(1,instr(m.text,?)-80),400) AS text
        FROM messages m JOIN sessions s ON s.id=m.session_id
        WHERE ${predicates.join(' AND ')} ORDER BY m.timestamp,m.uuid LIMIT ?`, request.query, ...args)};
    break;
  }
  case 'manifest': {
    // index_state enumerates actual indexed source files, including Codex child
    // files that are not standalone rows in sessions. Private cursor bytes are
    // carried unchanged; they are not parsed as portable identities.
    const predicates = [];
    const args = [request.after_path ?? ''];
    for (const source of binding.sources) {
      predicates.push('(i.jsonl_path=? OR substr(i.jsonl_path,1,length(?))=?)');
      args.push(source.path, source.prefix, source.prefix);
    }
    args.push(request.limit + 1);
    const rows = sql(`SELECT i.jsonl_path AS local_path,i.mtime AS indexed_mtime_ms,
      length(CAST(i.cursor AS BLOB)) AS index_cursor_bytes,i.replay_required,
      (SELECT CASE WHEN count(*)=1 THEN min(s.id) ELSE NULL END
       FROM sessions s WHERE s.jsonl_path=i.jsonl_path) AS session_id
      FROM index_state i
      WHERE i.jsonl_path>? AND (${predicates.join(' OR ')})
      ORDER BY i.jsonl_path LIMIT ?`, ...args);
    const has_more = rows.length > request.limit;
    manifest_rows = rows.slice(0, request.limit);
    manifest_next_path = has_more ? manifest_rows.at(-1)?.local_path ?? null : null;
    for (const row of manifest_rows) {
      // Metadata alone never claims to be the original payload. The caller
      // archives the entire file and binds references to that content version.
      row.records = row.session_id ? sql(`SELECT uuid AS id,role,timestamp,
        parent_uuid AS parent_id,agent_id,content_type,visibility
        FROM messages WHERE session_id=? AND agent_id IS NULL
        ORDER BY timestamp,uuid LIMIT 101`, row.session_id) : [];
      row.records_truncated = row.records.length > 100;
      row.records = row.records.slice(0, 100);
    }
    data = {next_path: manifest_next_path, snapshot_consistent_across_pages: false};
    break;
  }
  default:
    throw new Error('Unsupported Shenko Obelisk query operation');
}
return {schema_version: 1, data, manifest_rows, manifest_next_path,
  index_observation: {
    observed_at: new Date().toISOString(),
    sources: sql('SELECT source,count(*) AS sessions,max(ended_at) AS latest_session_time FROM sessions GROUP BY source'),
    refresh_complete: null,
    completeness_reason: 'The query API does not attest a complete inventory or a distributed frontier.',
    original_archive_complete: false,
    source_text_may_be_truncated: true,
  }};
