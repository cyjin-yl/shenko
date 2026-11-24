import { text as t, resolveMessage } from './messages';
import * as React from 'react';
import { RichDocument, type DocumentNodeView } from './document-view';
import { partState, type ActionReceiptView, type ConnectionState, type DegradedDocument, type DocumentErrorView, type Submission, type TimelinePart, type UiNode, type WorkView } from './projection';
import { Icon } from './icons';
import { Markdown } from './markdown';
import { Button, CollapsibleRoot, CollapsibleTrigger, CollapsibleContent, Separator, dayLabel, formatTime, type Emit } from './ui-kit';
import { isPage } from './slots';
import { DelegationBranches } from './delegation-branches';

export function EmptyTimeline({ connection, demo }: { connection: ConnectionState; demo: boolean; name?: string }) {
  return (
    <div className="empty" data-testid="empty-timeline">
      <Icon name="timeline" size={22} />
      <p>
        {demo
          ? t('timeline.emptyDemo')
          : connection === 'online'
            ? t('timeline.empty')
            : t('timeline.offline')}
      </p>
    </div>
  );
}

function entryTime(entry: UiNode): string | null {
  if (entry.kind === 'subject_part') return entry.props.part.observed_at;
  if (entry.kind === 'user_submission') return entry.props.submission.created_at;
  return null;
}

export function TimelineEntries({ entries, emit, name, onOpenWork }: {
  entries: UiNode[]; emit: Emit; name?: string; onOpenWork: (artifactId: string) => void;
}) {
  let lastDay = '';
  let previous: { kind: string; at: number } | undefined;
  return <div className="entries">{entries.map((entry) => {
    // A document (or its failure) rides the day of the committed step that
    // carried it; it never opens a t('time.unknown') group of its own.
    if (entry.kind === 'semantic_document' || entry.kind === 'document_error' || entry.kind === 'work') {
      previous = undefined;
      return <EntryView node={entry} emit={emit} key={entry.node_id} name={name} continued={false} onOpenWork={onOpenWork} />;
    }
    const value = entryTime(entry);
    const day = dayLabel(value);
    const separator = day !== lastDay
      ? <div className="day" key={`day-${entry.node_id}`}><Separator className="day-rule" /><span>{day}</span><Separator className="day-rule" /></div>
      : null;
    if (separator) previous = undefined;
    lastDay = day;
    // Consecutive subject rounds within a few minutes read as one voice: the
    // seal and name are shown once, each round keeps its own meta line.
    const at = value ? Date.parse(value) : Number.NaN;
    const continued = previous !== undefined
      && previous.kind === entry.kind
      && Number.isFinite(at)
      && at - previous.at < 6 * 60_000;
    previous = { kind: entry.kind, at };
    return <React.Fragment key={entry.node_id}>
      {separator}
      <EntryView node={entry} emit={emit} name={name} continued={continued} onOpenWork={onOpenWork} />
    </React.Fragment>;
  })}</div>;
}

function EntryView({ node, emit, name, continued, onOpenWork }: {
  node: UiNode; emit: Emit; name?: string; continued: boolean; onOpenWork: (artifactId: string) => void;
}) {
  switch (node.kind) {
    case 'subject_part': return <SubjectPart node={node} emit={emit} name={name} continued={continued} />;
    case 'semantic_document': return <SemanticDocumentView node={node} emit={emit} />;
    case 'document_error': return <DocumentErrorEntry node={node} />;
    case 'work': return <WorkEntry node={node} emit={emit} onOpenWork={onOpenWork} />;
    default: return <UserSubmission node={node} emit={emit} continued={continued} />;
  }
}

function SubjectPart({ node, emit, name, continued }: { node: UiNode; emit: Emit; name?: string; continued: boolean }) {
  const part = node.props.part as TimelinePart;
  const state = partState(part);
  const live = state.streaming && !part.terminal;
  // Only committed text carries a step number. A durable streamed prefix is
  // real ledger content, but labelling it "第 N 轮" would claim an S1 step the
  // part does not have yet.
  const stepLabel = state.committed ? t('timeline.round', { step: part.step }) : part.terminal ? t('timeline.streamEnded') : t('timeline.streaming');
  const announcement = state.committed
    ? t('timeline.committedAnnouncement', { step: part.step })
    : t('timeline.streamAnnouncement');
  return (
    <article
      className={`entry subject${continued ? ' is-continued' : ''}`}
      data-entry-key={node.node_id}
      data-message-id={part.message_id}
      data-part-id={part.part_id}
      data-part-revision={part.part_revision}
      data-next-chunk-seq={part.next_chunk_seq}
      data-commit-ref={part.commit_ref}
      data-committed={state.committed ? 'true' : 'false'}
      data-streaming={state.streaming ? 'true' : 'false'}
      aria-label={announcement}
      aria-busy={live ? true : undefined}
    >
      <div className="entry-main">
        <header className="entry-meta">
          <span className={`entry-step${state.committed ? '' : ' is-live'}`}>
            {live && <span className="live-dot" aria-hidden="true" />}
            {stepLabel}
          </span>
          <time dateTime={part.observed_at} title={t('timeline.receivedTime')}>{formatTime(part.observed_at)}</time>
          {!state.committed && <span className="tag tag-quiet">{t('timeline.uncommitted')}</span>}
          {part.gap && <span className="tag tag-warn">{t('timeline.gap')}</span>}
        </header>
        <div className="entry-body">
          <PartErrorBoundary text={part.content}><Markdown text={part.content} /></PartErrorBoundary>
        </div>
        {state.committed && <DelegationBranches stepId={part.step_id} />}
        <footer className="entry-tools">
          <Button variant="ghost" size="sm" label={t('timeline.copyRound')} onClick={() => emit(node.node_id, node.action_refs[0])}>
            <Icon name="copy" size={15} /><span>{t('common.copy')}</span>
          </Button>
          <CollapsibleRoot className="identity">
            <CollapsibleTrigger className="identity-trigger">
              <Icon name="info" size={15} /><span>{t('common.source')}</span>
            </CollapsibleTrigger>
            <CollapsibleContent className="identity-body"><dl>
              <div><dt>message</dt><dd>{part.message_id}</dd></div>
              <div><dt>part</dt><dd>{part.part_id}</dd></div>
              <div><dt>revision</dt><dd>{part.part_revision}</dd></div>
              <div><dt>commit</dt><dd>{part.commit_ref}</dd></div>
              <div><dt>request / attempt</dt><dd>{part.request_id ?? t('common.unknown')} / {part.attempt_id ?? t('common.unknown')}</dd></div>
              <div><dt>{t('common.state')}</dt><dd>{state.committed ? t('timeline.committed') : t('timeline.durableUncommitted')}</dd></div>
            </dl></CollapsibleContent>
          </CollapsibleRoot>
        </footer>
      </div>
    </article>
  );
}

class PartErrorBoundary extends React.Component<{ text: string; children: React.ReactNode }, { failed: boolean }> {
  state = { failed: false };
  static getDerivedStateFromError() { return { failed: true }; }
  render() {
    if (this.state.failed) {
      return <div className="part-fallback" role="note">
        <p>{t('timeline.fallback')}</p>
        <pre>{this.props.text}</pre>
        <button type="button" onClick={() => this.setState({ failed: false })}>{t('timeline.rerender')}</button>
      </div>;
    }
    return this.props.children;
  }
}

// Durable admission stages are distinct facts
// each label names one stage; none is drawn as a progress bar.
const RECEIPT: Record<string, { short: string; long: string; tone: string }> = {
  sending: { short: t('receipt.sending'), long: t('receipt.writing'), tone: 'pending' },
  sent: { short: t('receipt.sent'), long: t('receipt.awaiting'), tone: 'pending' },
  accepted: { short: t('receipt.accepted'), long: t('receipt.acceptedDetail'), tone: 'accepted' },
  selected: { short: t('receipt.selected'), long: t('receipt.selectedDetail'), tone: 'accepted' },
  committed: { short: t('receipt.committed'), long: t('receipt.committedDetail'), tone: 'done' },
  conflict: { short: t('receipt.conflict'), long: t('receipt.conflictDetail'), tone: 'error' },
  rejected: { short: t('receipt.rejected'), long: t('receipt.rejectedDetail'), tone: 'error' },
  unknown: { short: t('receipt.unknown'), long: t('receipt.unknownDetail'), tone: 'warn' },
};

function placementText(submission: Submission): string | null {
  const placement = submission.placement;
  if (!placement) return null;
  switch (placement.actual) {
    case 'next_boundary': return t('receipt.nextBoundary');
    case 'live_steer': return t('receipt.liveSteer');
    case 'cancel_reseal': return t('receipt.reseal');
    default: return t('receipt.pendingPlacement');
  }
}

function UserSubmission({ node, emit, continued }: { node: UiNode; emit: Emit; continued: boolean }) {
  const submission = node.props.submission as Submission;
  const receipt = RECEIPT[submission.receipt] ?? RECEIPT.unknown;
  const withdrawal = submission.withdrawal;
  const withdrawAction = node.props.withdraw_action as string | undefined;
  const retryAction = node.props.retry_action as string | undefined;
  const editAction = node.props.edit_action as string | undefined;
  const placement = placementText(submission);
  const steer = Boolean(submission.placement?.target_request_id);
  const detail = [receipt.long, resolveMessage(submission.detail), placement].filter(Boolean).join(' · ');
  return (
    <article
      className={`entry user${continued ? ' is-continued' : ''}${withdrawal ? ' is-withdrawn' : ''}`}
      data-entry-key={node.node_id}
      data-submission-id={submission.payload.submission_id}
    >
      <div className="entry-main">
        <div className="bubble">
          {steer && <span className="bubble-mode"><Icon name="steer" size={13} />{t('composer.steer')}</span>}
          <div className="entry-body">{submission.payload.content}</div>
        </div>
        <footer className={`receipt tone-${receipt.tone}`} data-receipt={submission.receipt} title={detail}>
          <span className="receipt-dot" aria-hidden="true" />
          <span className="receipt-label">{receipt.short}</span>
          <span className="sr-only">{t('common.colon')}{detail}</span>
          {submission.detail && <span className="receipt-detail">{resolveMessage(submission.detail)}</span>}
          {placement && <span className="receipt-detail">{placement}</span>}
          {withdrawal && (
            <span className="receipt-detail is-withdrawal">
              {withdrawal.result === 'withdrawal_queued' ? t('receipt.withdrawalQueued') : t('receipt.withdrawnBefore')}
            </span>
          )}
          <time dateTime={submission.created_at}>{formatTime(submission.created_at)}</time>
          {(submission.receipt === 'rejected' || submission.receipt === 'unknown') && typeof retryAction === 'string' && (
            <button type="button" className="text-action" onClick={() => emit(node.node_id, retryAction)}>
              <Icon name="retry" size={14} />{t('receipt.retrySame')}
            </button>
          )}
          {!withdrawal && typeof withdrawAction === 'string' && (
            <button type="button" className="text-action" onClick={() => emit(node.node_id, withdrawAction)}>
              <Icon name="undo" size={14} />{t('common.withdraw')}
            </button>
          )}
          {typeof editAction === 'string' && (
            <button type="button" className="text-action" onClick={() => emit(node.node_id, editAction)}>
              {t('common.edit')}
            </button>
          )}
        </footer>
      </div>
    </article>
  );
}

/// One work of the subject, in the timeline.
///
/// It names the work's own revision: what the reader does next is bound to
/// the revision they saw. Opening the work is the reader's own action.
function WorkEntry({ node, emit, onOpenWork }: { node: UiNode; emit: Emit; onOpenWork: (artifactId: string) => void }) {
  const work = node.props.work as WorkView;
  // Page works offer both the live page and their revision-bound management
  // record; all work actions still use the projected action identity.
  const page = isPage(work.mime);
  return (
    <article className="entry attachment" data-entry-key={node.node_id} data-artifact-id={work.artifact_id}>
      <div className="entry-main">
        <div className="work-card">
          <span className="work-icon"><Icon name="file" /></span>
          <span className="work-text">
            <strong>{String(work.title ?? work.artifact_id)}</strong>
            <span className="work-meta">
              {t('work.summary', { revision: work.revision ?? '—', lifecycle: work.lifecycle ?? 'draft', mime: work.mime ?? '—' })}
            </span>
          </span>
          <Button
            variant="outline"
            size="sm"
            onClick={() => page ? onOpenWork(work.artifact_id) : emit(node.node_id, node.action_refs[0])}
          >
            <Icon name="open" size={15} />{t('common.open')}
          </Button>
          {page && <Button variant="ghost" size="sm" onClick={() => emit(node.node_id, node.action_refs[0])}>
            {t('work.details')}
          </Button>}
        </div>
      </div>
    </article>
  );
}

// The rich layer is gone; the committed text beside it stays the record and
// the reason is readable instead of only flashing as a notice (docs/research/product-positioning.md#spec-3).
function DocumentErrorEntry({ node }: { node: UiNode }) {
  const error = node.props.error as DocumentErrorView;
  return (
    <article
      className="entry attachment"
      data-entry-key={node.node_id}
      data-source-step={error.source_step_id}
      role="note"
    >
      <div className="entry-main">
        <div className="doc-error">
          <Icon name="warning" />
          <div>
            <p>{t('document.failed')}</p>
            <p className="doc-error-reason">{error.reason}</p>
            {error.source_commit && <code>commit {error.source_commit}</code>}
          </div>
        </div>
      </div>
    </article>
  );
}

// A degraded semantic document: the mode decides how much structure renders,
// but the numbered options are always the same action identities.
// Execution authority comes from the subject's disposition of the admitted
// input; this view only submits that input.
const ACTION_STAGE_LABELS: Record<ActionReceiptView['stage'], string> = {
  sending: t('action.sending'),
  accepted: t('action.accepted'),
  selected: t('action.selected'),
  committed: t('action.committed'),
  rejected: t('action.rejected'),
  unavailable: t('action.unavailable'),
  conflict: t('action.conflict'),
};

export function SemanticDocumentView({ node, emit }: { node: UiNode; emit: Emit }) {
  const document = node.props.document as DegradedDocument;
  const receipts = (node.props.receipts ?? []) as ActionReceiptView[];
  const latest = (actionId: string): ActionReceiptView | undefined => receipts
    .filter((row) => row.document_id === document.document_id
      && row.revision === document.revision && row.action_id === actionId)
    .at(-1);
  // An admitted decision awaiting disposition must not be admitted twice by a
  // second click; the option stays visible with its durable stage shown.
  const admitted = (row: ActionReceiptView | undefined): boolean =>
    row !== undefined && ['sending', 'accepted', 'selected'].includes(row.stage);
  return (
    <article className="entry attachment" data-entry-key={node.node_id} data-document-id={document.document_id}>
      <div className="entry-main">
        <div className={`doc is-${document.mode}`}>
          <header className="doc-head">
            <span className="doc-kind"><Icon name="spark" size={15} />{t('document.ui')}</span>
            <span className="doc-mode" data-mode={document.mode}>
              {document.mode === 'rich' ? t('document.rich') : document.mode === 'card' ? t('document.card') : t('document.text')}
            </span>
          </header>
          <p className="doc-summary">{document.summary}</p>
          {document.mode === 'rich' && Array.isArray(document.nodes) && document.nodes.length > 0 && (
            <RichDocument
              nodes={document.nodes as DocumentNodeView[]}
              options={document.options}
              actionRefs={node.action_refs ?? []}
              onAction={(actionRef) => emit(node.node_id, actionRef)}
              renderMarkdown={(markdown) => <Markdown text={markdown} />}
            />
          )}
          {document.options.length > 0 && (
            <ol className="doc-options">
              {document.options.map((option, index) => {
                const receipt = latest(option.action_id);
                const pending = admitted(receipt);
                return (
                  <li key={`${option.action_id}:${option.nonce}`} data-action-id={option.action_id}>
                    <button
                      type="button"
                      className="doc-option"
                      disabled={pending || !node.action_refs[index]}
                      onClick={() => emit(node.node_id, node.action_refs[index])}
                    >
                      <span className="doc-option-index">{index + 1}</span>
                      <span className="doc-option-label">{option.label}</span>
                      {receipt && (
                        <span className={`doc-option-receipt is-${receipt.stage}`} data-stage={receipt.stage}>
                          {ACTION_STAGE_LABELS[receipt.stage]}{receipt.detail ? ` · ${resolveMessage(receipt.detail)}` : ''}
                        </span>
                      )}
                    </button>
                  </li>
                );
              })}
            </ol>
          )}
        </div>
      </div>
    </article>
  );
}
