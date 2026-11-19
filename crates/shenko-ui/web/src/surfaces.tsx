import { text as t } from './messages';
import * as React from 'react';
import type {
  BuildManifest,
  OpenedWork,
  OwnerState,
  PlaygroundView,
  TimelinePart,
  UiNode,
  UiProjection,
} from './projection';
import { ConfigEntries } from './config-panel';
import { taskStateLabel } from './threads';
import {
  readContentRevisionDisclosure,
  readRevisionDisclosure,
  type ContentRevisionDisclosure,
  type RevisionDisclosure,
} from './revision-disclosure';
import { Icon } from './icons';
import { isPage } from './slots';
import {
  Button,
  CollapsibleContent,
  CollapsibleRoot,
  CollapsibleTrigger,
  DrawerContent,
  DrawerDescription,
  DrawerHeader,
  DrawerMask,
  DrawerRoot,
  DrawerTitle,
  Note,
  ProtoInput,
  connectionAnnouncement,
  firstLine,
  formatStamp,
  formatStampMs,
  formatTime,
  type Emit,
} from './ui-kit';

/**
 * The on-demand surfaces: one page
 * shape for all of them — a title that says what the page is, then plain
 * rows. Every value is the interface's own; empty, unavailable and error are
 * three different sentences, never a placeholder row.
 */
function Page({ title, lead, hidden, className, children }: {
  title: string; lead: string; hidden: boolean; className?: string; children: React.ReactNode;
}) {
  return (
    <section className={['page', className].filter(Boolean).join(' ')} hidden={hidden} aria-label={title}>
      <div className="page-column">
        <header className="page-head">
          <h2>{title}</h2>
          <p>{lead}</p>
        </header>
        {children}
      </div>
    </section>
  );
}

function Group({ title, children, aside }: { title: string; children: React.ReactNode; aside?: React.ReactNode }) {
  return (
    <section className="group">
      <header className="group-head"><h3>{title}</h3>{aside}</header>
      {children}
    </section>
  );
}

export function TasksSurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const data = node.props.data;
  const items = Array.isArray(data?.items) ? data.items : [];
  return (
    <Page title={t('config.tasks')} lead={t('config.tasksLead')} hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text={t('config.tasksUnavailable')} />
        : data?.available === false
          ? <Note text={data?.note || t('config.tasksMissing')} />
          : items.length === 0
            ? <Note text={data?.note || t('config.tasksEmpty')} />
            : <ul className="rows">{items.map((task: Record<string, unknown>, index: number) => (
              <li className="row" key={String(task.task_id ?? index)}>
                <span className="row-main">
                  <span className="row-title">{String(task.kind ?? '—')}</span>
                  <code className="row-sub">{String(task.task_id ?? '—')}</code>
                </span>
                <span className="row-state" data-state={String(task.state ?? 'unknown')}>{taskStateLabel(task.state)}</span>
                <time className="row-time">{formatStamp(task.created_at)}</time>
              </li>
            ))}</ul>}
    </Page>
  );
}

export function MemorySurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const items = Array.isArray(node.props.data?.items) ? node.props.data.items : [];
  return (
    <Page title={t('config.memory')} lead={t('config.memoryLead')} hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text={t('config.memoryUnavailable')} />
        : items.length === 0
          ? <Note text={node.props.data?.note || t('config.memoryEmpty')} />
          : <ul className="rows">{items.map((memory: Record<string, unknown>, index: number) => (
            <li className="row is-stacked" key={String(memory.id ?? index)}>
              <span className="row-main">
                <span className="row-text">{String(memory.content ?? memory.text ?? memory.summary ?? memory.title ?? '—')}</span>
                <span className="row-sub">
                  {/* `kind` is a learnable hint, never the entry's content. */}
                  {memory.kind ? <span>{String(memory.kind)}</span> : null}
                  <time>{formatStamp(memory.created_at)}</time>
                </span>
                <RevisionDisclosureView disclosure={readRevisionDisclosure(memory.disclosure)} />
                <ContentRevisionDisclosureView revision={readContentRevisionDisclosure(memory.revision_disclosure)} />
              </span>
            </li>
          ))}</ul>}
    </Page>
  );
}

/**
 * Renders the server's taste disclosure verbatim: retained-version count and
 * latest modification by default, full text only when the request asked for a
 * specific revision. A missing projection stays missing.
 */
function RevisionDisclosureView({ disclosure }: { disclosure: RevisionDisclosure | null }) {
  if (!disclosure) return null;
  const facts: string[] = [];
  if (disclosure.older_versions !== null) facts.push(t('revision.older', { count: disclosure.older_versions }));
  if (disclosure.latest_modified_at_ms !== null) facts.push(t('revision.modified', { time: formatStampMs(disclosure.latest_modified_at_ms) }));
  if (disclosure.selected !== null) facts.push(t('revision.selected', { revision: disclosure.selected }));
  if (disclosure.written_at_ms !== null) facts.push(t('revision.written', { time: formatStampMs(disclosure.written_at_ms) }));
  if (facts.length === 0 && !disclosure.full_text) return null;
  return <div className="revision" data-revision-family={disclosure.family}>
    {facts.length ? <span>{facts.join(' · ')}</span> : null}
    {disclosure.content_digest ? <code>{disclosure.content_digest.slice(0, 12)}</code> : null}
    {disclosure.full_text ? <div className="revision-full">
      <h4>{t('revision.full', { revision: disclosure.full_text.revision, written: disclosure.full_text.written_at_ms !== null ? ` · ${t('revision.written', { time: formatStampMs(disclosure.full_text.written_at_ms) })}` : '' })}</h4>
      {disclosure.full_text.text !== null
        ? <p>{disclosure.full_text.text}</p>
        : <small>{t('revision.unparsed')}</small>}
    </div> : null}
    {disclosure.full_text_unreadable ? <small>{t('revision.unreadable')}</small> : null}
  </div>;
}

/** Store-level revision summary exactly as projected; no derived chain. */
function ContentRevisionDisclosureView({ revision }: { revision: ContentRevisionDisclosure | null }) {
  if (!revision) return null;
  return <div className="revision">
    <span>{t('revision.summary', { count: revision.old_version_count, time: formatStampMs(revision.latest_modified_at_ms) })}</span>
    <code>{revision.latest_digest.slice(0, 12)}</code>
  </div>;
}

/** Compiler facts exactly as the build recorded them; nothing is inferred. */
function compilerLabel(compiler: BuildManifest['compiler'] | undefined): string {
  if (!compiler) return t('compiler.missing');
  const runtime = compiler.runtime_imported === true ? t('compiler.imported') : t('compiler.notImported');
  if (compiler.status !== 'build_time_compiled') return t('compiler.status', { status: compiler.status, runtime });
  const names = (compiler.prototypes ?? []).map((prototype) => prototype.name);
  const profile = compiler.profile ?? t('compiler.profileUnknown');
  if (names.length === 0) return t('compiler.noPrototypes', { profile, runtime });
  return t('compiler.prototypes', { count: names.length, names: names.join('、'), profile, runtime });
}

function Fact({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return <div className="fact"><dt>{label}</dt><dd className={mono ? 'is-mono' : undefined} title={value}>{value}</dd></div>;
}

function wsUrl(): string {
  if (!location.host) return t('common.unavailable');
  return `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws`;
}

export function ConfigSurface({ node, projection, emit }: { node?: UiNode; projection: UiProjection; emit: Emit }) {
  if (!node) return null;
  const config = node.props.config;
  const self = node.props.self;
  const manifest = node.props.manifest as BuildManifest | undefined;
  const unknown = (value: unknown) => value === undefined || value === null || value === '' || value === 'unknown' ? t('common.unknown') : String(value);
  const caps = projection.capabilities;
  return (
    <Page title={t('config.title')} lead={t('config.lead')} hidden={node.props.hidden} className="config">
      <Group title={t('config.runtime')}>
        <dl className="facts">
          <Fact label={t('config.model')} value={node.props.config_error ? t('config.apiUnavailable') : unknown(config?.model)} />
          <Fact label={t('config.subjectState')} value={unknown(self?.status)} />
          <Fact label={t('config.transport')} value={connectionAnnouncement(node.props.connection)} />
          <Fact label={t('config.latestCommit')} value={node.props.step ? t('config.commit', { step: node.props.step }) : t('config.noCommit')} />
          <Fact label="Runtime generation" value={unknown(config?.generation)} mono />
          <Fact label="API key" value={typeof config?.api_key_configured === 'boolean' ? config.api_key_configured ? t('config.keyConfigured') : t('config.keyMissing') : t('common.unknown')} />
          <Fact label={t('config.serverVersion')} value={unknown(node.props.server_version)} mono />
          <Fact label={t('config.dataDirectory')} value={unknown(config?.data_dir)} mono />
          <Fact label="WebSocket" value={wsUrl()} mono />
          <Fact label="Subject ID" value={projection.subject_id ?? t('common.unknown')} mono />
        </dl>
      </Group>
      <Group title={t('config.entries')}>
        <ConfigEntries node={node} emit={emit} />
      </Group>
      <Group title={t('config.inputCapabilities')}>
        <dl className="facts">
          <Fact label={t('composer.append')} value={caps.append ? caps.durable_input_receipts ? t('config.durableAvailable') : t('config.transportAvailable') : t('common.unavailable')} />
          <Fact label={t('config.steer')} value={caps.steer ? t('config.boundaryAvailable') : t('config.notOffered')} />
          <Fact label={t('config.selectionReceipt')} value={caps.durable_input_receipts ? t('config.storeReceipt') : t('common.unavailable')} />
          <Fact label={t('config.streaming')} value={caps.streaming ? t('config.streamingAvailable') : t('config.committedOnly')} />
          <Fact label={t('config.replay')} value={caps.committed_replay ? t('config.connected') : t('common.unknown')} />
        </dl>
      </Group>
      <Group title={t('config.frontendBuild')}>
        {manifest ? <dl className="facts">
          <Fact label="Proto-UI" value={manifest.upstream.commit} mono />
          <Fact label={t('config.packages')} value={manifest.packages.join(' · ')} mono />
          <Fact label={t('config.families')} value={manifest.adopted_families.join('、')} />
          <Fact label="Compiler" value={compilerLabel(manifest.compiler)} />
        </dl> : <Note text={t('config.manifestMissing')} />}
        <p className="links">
          <a href="/assets/licenses/PROTO-UI-LICENSE" target="_blank" rel="noreferrer">{t('config.protoLicense')}</a>
          <a href="/assets/licenses/SHADCN-NOTICES.md" target="_blank" rel="noreferrer">{t('config.shadcnLicense')}</a>
          <a href="/assets/licenses/REACT-LICENSE" target="_blank" rel="noreferrer">{t('config.reactLicense')}</a>
        </p>
      </Group>
    </Page>
  );
}

export function PlaygroundSurface({ node, onOpenWork }: { node?: UiNode; onOpenWork?: (artifactId: string) => void }) {
  if (!node) return null;
  const playground: PlaygroundView | undefined = node.props.playground;
  const roots = Array.isArray(playground?.roots) ? playground.roots : [];
  const artifacts = Array.isArray(playground?.artifacts) ? playground.artifacts : [];
  const namespaces = Array.isArray(playground?.namespaces) ? playground.namespaces : [];
  return (
    <Page title="Playground" lead={t('work.playgroundLead')} hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text={t('work.apiUnavailable')} />
        : playground?.available === false
          ? <Note text={t('work.storeRequired')} />
          : <>
            <Group title={t('work.title')}>
              {artifacts.length === 0 ? <Note text={t('work.empty')} /> : (
                <ul className="rows">{artifacts.map((artifact) => (
                  <li className="row" key={String(artifact.artifact_id)}>
                    <Icon name="file" />
                    <span className="row-main">
                      <span className="row-title">{String(artifact.title ?? artifact.artifact_id)}</span>
                      <span className="row-sub">
                        r{String(artifact.revision ?? '—')} · {String(artifact.mime ?? '—')} · {artifact.backend ? t('work.backend') : t('work.content')}
                      </span>
                    </span>
                    <span className="row-state">{String(artifact.lifecycle ?? 'draft')}</span>
                    {isPage(artifact.mime) && onOpenWork
                      ? <Button variant="outline" size="sm" onClick={() => onOpenWork(String(artifact.artifact_id))}>
                        <Icon name="open" size={15} />打开
                      </Button>
                      : <time className="row-time">{formatStamp(artifact.updated_at)}</time>}
                  </li>
                ))}</ul>
              )}
            </Group>
            <Group title={t('work.rootTitle')}>
              {roots.length === 0 ? <Note text={t('work.rootsEmpty')} /> : (
                <ul className="rows">{roots.map((root) => (
                  <li className="row" key={String(root.artifact_id)}>
                    <span className="row-main">
                      <span className="row-title">{String(root.root)}</span>
                      <code className="row-sub">{String(root.artifact_id)} · {String(root.identity).slice(0, 16)}…</code>
                    </span>
                  </li>
                ))}</ul>
              )}
            </Group>
            <Group title={t('work.namespaces')}>
              {namespaces.length === 0 ? <Note text={t('work.namespacesEmpty')} /> : (
                <ul className="rows">{namespaces.map((entry) => (
                  <li className="row" key={String(entry.namespace)}>
                    <span className="row-main">
                      <code className="row-title">{String(entry.namespace)}</code>
                      <span className="row-sub">{String(entry.description ?? '—')}</span>
                    </span>
                  </li>
                ))}</ul>
              )}
            </Group>
          </>}
    </Page>
  );
}

const SECTION_LABELS: Record<string, string> = { status: t('config.currentStatus'), activity: t('config.recentCommits'), artifacts: t('config.artifacts'), devices: t('config.devices') };

/**
 * The subject's details, as an on-demand drawer (the upstream shadcn drawer
 * family owns focus containment, Escape and the mask).
 */
export function ContextPanel({ node, projection, emit }: { node: UiNode; projection: UiProjection; emit: Emit }) {
  const open = Boolean(node.props.open);
  const name = projection.identity?.display_name;
  return (
    <DrawerRoot
      className="sheet-root"
      open={open}
      a11yLabel={t('subject.details')}
      onOpenChange={(detail: { open: boolean }) => {
        if (detail.open !== open) emit('context', 'panel.toggle');
      }}
    >
      <DrawerMask className="sheet-mask" />
      <DrawerContent side="right" className="sheet">
        <DrawerHeader className="sheet-head">
          <div className="sheet-title">
            <DrawerTitle className="sheet-name">{name || t('subject.unnamed')}</DrawerTitle>
            <DrawerDescription className="sheet-sub">
              {projection.identity?.revision ? t('identity.revision', { revision: projection.identity.revision }) : t('identity.unknownRevision')}
            </DrawerDescription>
          </div>
          <Button variant="ghost" size="icon" label={t('identity.close')} onClick={() => emit('context', 'panel.toggle')}>
            <Icon name="close" />
          </Button>
        </DrawerHeader>
        <div className="sheet-body">
          <OpenedWorkPanel node={node} emit={emit} />
          {(node.props.sections as UiNode[]).map((section) => (
            <PanelSection key={section.node_id} section={section} node={node} emit={emit} />
          ))}
          <IdentityPanel node={node} emit={emit} />
        </div>
      </DrawerContent>
    </DrawerRoot>
  );
}

/// The instance's own name and who owns it.
///
/// The name is the instance's, not the brand's, and the reader may give it one
/// — or let the subject name itself. Ownership is reported as the fact it is:
/// claimed, not claimed, or a claim still pending.
function IdentityPanel({ node, emit }: { node: UiNode; emit: Emit }) {
  const owner = node.props.owner_state as OwnerState | undefined;
  const nameDraft = (node.props.name_draft ?? '') as string;
  const claimDraft = (node.props.claim_draft ?? '') as string;
  const nameRef = node.props.identity_name_ref as string | undefined;
  const claimRef = node.props.owner_claim_ref as string | undefined;
  return (
    <section className="sheet-section">
      <h3>{t('identity.instance')}</h3>
      <div className="field">
        <label className="field-label" id="identity-name-label">{t('identity.name')}</label>
        <div className="field-row">
          <ProtoInput
            className="input"
            labelledBy="identity-name-label"
            value={nameDraft}
            placeholder={t('identity.namePlaceholder')}
            onValueChange={(detail: { value: string }) => emit(node.node_id, node.props.identity_name_draft_ref ?? '', { value: detail.value })}
          />
          <Button variant="outline" size="sm" onClick={() => nameRef && emit(node.node_id, nameRef)}>{t('identity.setName')}</Button>
        </div>
      </div>
      {owner?.available === false ? (
        <Note text={t('owner.unavailable')} />
      ) : (
        <div className="field">
          <span className="field-label" id="owner-claim-label">
            {t('owner.status', { state: owner?.claimed === true ? t('owner.claimed') : owner?.claim_pending === true ? t('owner.pending') : t('owner.unclaimed') })}
          </span>
          {!owner?.claimed && (
            <div className="field-row">
              <ProtoInput
                className="input"
                labelledBy="owner-claim-label"
                value={claimDraft}
                placeholder={t('owner.credential')}
                onValueChange={(detail: { value: string }) => emit(node.node_id, node.props.owner_claim_draft_ref ?? '', { value: detail.value })}
              />
              <Button variant="outline" size="sm" onClick={() => claimRef && emit(node.node_id, claimRef)}>{t('owner.claim')}</Button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

/// The work the reader opened, with the actions bound to its revision
/// Every action names the revision the reader saw: a work
/// that moved on refuses the stale action instead of quietly applying to
/// whatever is there now.
function OpenedWorkPanel({ node, emit }: { node: UiNode; emit: Emit }) {
  const work = node.props.work as OpenedWork | undefined;
  const refs = (node.props.work_action_refs ?? []) as string[];
  if (node.props.work_error === true) {
    return <section className="sheet-section"><Note tone="error" text={t('work.unreadable')} /></section>;
  }
  if (!work) return null;
  return (
    <section className="sheet-section work" data-artifact-id={work.artifact_id}>
      <h3>{String(work.title ?? work.artifact_id)}</h3>
      <p className="sheet-sub">r{String(work.revision ?? '—')} · {String(work.lifecycle ?? 'draft')} · {String(work.mime ?? '—')}</p>
      {work.content !== undefined && <pre className="work-content" tabIndex={0}><code>{work.content}</code></pre>}
      <div className="button-row">
        <Button variant="outline" size="sm" onClick={() => refs[0] && emit(node.node_id, refs[0])}>{t('work.stage')}</Button>
        <Button variant="outline" size="sm" onClick={() => refs[1] && emit(node.node_id, refs[1])}>{t('work.adopt')}</Button>
        <Button variant="ghost" size="sm" onClick={() => refs[2] && emit(node.node_id, refs[2])}>{t('work.rollback')}</Button>
      </div>
    </section>
  );
}

function PanelSection({ section, node, emit }: { section: UiNode; node: UiNode; emit: Emit }) {
  const name = section.props.section as string;
  const open = Boolean(section.props.open);
  return (
    <CollapsibleRoot
      open={open}
      className="disclosure"
      onOpenChange={(detail: { open: boolean }) => {
        if (detail.open !== open) emit(section.node_id, section.action_refs[0], { section: name });
      }}
    >
      <CollapsibleTrigger className="disclosure-trigger">
        <Icon name="chevron" size={15} />
        <span>{SECTION_LABELS[name]}</span>
      </CollapsibleTrigger>
      <CollapsibleContent className="disclosure-body"><PanelSectionBody name={name} node={node} /></CollapsibleContent>
    </CollapsibleRoot>
  );
}

function PanelSectionBody({ name, node }: { name: string; node: UiNode }) {
  const self = node.props.self;
  const config = node.props.config;
  if (name === 'status') return <dl className="facts is-compact">
    <Fact label={t('config.subjectState')} value={self?.status && self.status !== 'unknown' ? self.status : t('common.unknown')} />
    <Fact label={t('config.transport')} value={connectionAnnouncement(node.props.connection)} />
    <Fact label={t('config.latestCommit')} value={node.props.step ? t('config.commit', { step: node.props.step }) : t('config.noCommit')} />
    <Fact label={t('config.clients')} value={typeof self?.connected_clients === 'number' ? String(self.connected_clients) : t('common.unknown')} />
    <Fact label={t('config.model')} value={config?.model && config.model !== 'unknown' ? config.model : t('common.unknown')} />
  </dl>;
  if (name === 'activity') {
    const parts = (node.props.timeline as TimelinePart[]).filter((part) => part.content).slice(-3).reverse();
    return parts.length
      ? <ul className="mini-rows">{parts.map((part) => <li key={part.part_id}>
        <span className="mini-meta">{t('timeline.round', { step: part.step })} · {formatTime(part.observed_at)}</span>
        <span className="mini-text">{firstLine(part.content)}</span>
      </li>)}</ul>
      : <p className="muted">{t('config.noCommittedContent')}</p>;
  }
  if (name === 'artifacts') {
    if (typeof self?.artifacts_error === 'string' && self.artifacts_error) {
      return <p className="muted" role="status">{t('config.artifactsUnavailable')}</p>;
    }
    const artifacts = Array.isArray(self?.artifacts) ? self.artifacts : [];
    return artifacts.length
      ? <ul className="mini-rows">{artifacts.map((artifact: { id: string; title?: string; kind?: string }) => <li key={artifact.id}>
        <span className="mini-text">{artifact.title || artifact.id}</span>
        <span className="mini-meta">{artifact.kind || t('config.unknownType')}</span>
      </li>)}</ul>
      : <p className="muted">{t('config.artifactsEmpty')}</p>;
  }
  const devices = Array.isArray(self?.devices) ? self.devices : [];
  return devices.length
    ? <ul className="mini-rows">{devices.map((device: { id: string; name?: string; label?: string; kind?: string; online?: boolean }) => <li key={device.id}>
      <span className="mini-text">{device.name || device.label || device.id}</span>
      <span className="mini-meta">{device.online === true ? t('config.online') : device.online === false ? t('transport.offline') : t('config.stateUnknown')}{device.kind ? ` · ${device.kind}` : ''}</span>
    </li>)}</ul>
    : <p className="muted">{t('config.devicesUnavailable')}</p>;
}
