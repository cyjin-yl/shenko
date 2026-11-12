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
import {
  readContentRevisionDisclosure,
  readRevisionDisclosure,
  type ContentRevisionDisclosure,
  type RevisionDisclosure,
} from './revision-disclosure';
import { Icon } from './icons';
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
 * The on-demand surfaces (docs/specs/web-interface.md#spec-21-1): one page
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
    <Page title="任务" lead="当前节点的任务记录。" hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text="任务接口不可用。" />
        : data?.available === false
          ? <Note text={data?.note || '任务源尚未接入。'} />
          : items.length === 0
            ? <Note text={data?.note || '暂无任务。'} />
            : <ul className="rows">{items.map((task: Record<string, unknown>, index: number) => (
              <li className="row" key={String(task.task_id ?? index)}>
                <span className="row-main">
                  <span className="row-title">{String(task.kind ?? '—')}</span>
                  <code className="row-sub">{String(task.task_id ?? '—')}</code>
                </span>
                <span className="row-state" data-state={String(task.state ?? 'unknown')}>{String(task.state ?? '未知')}</span>
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
    <Page title="记忆" lead="接口实际返回的记忆条目；未持久化的内容不会出现在这里。" hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text="记忆接口不可用。" />
        : items.length === 0
          ? <Note text={node.props.data?.note || '暂无记忆条目。'} />
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
 * specific revision. A missing projection stays missing (docs/specs/cognition-units.md#spec-51-1, docs/specs/cognition-units.md#spec-51-4).
 */
function RevisionDisclosureView({ disclosure }: { disclosure: RevisionDisclosure | null }) {
  if (!disclosure) return null;
  const facts: string[] = [];
  if (disclosure.older_versions !== null) facts.push(`旧版本 ${disclosure.older_versions} 版`);
  if (disclosure.latest_modified_at_ms !== null) facts.push(`最新修改 ${formatStampMs(disclosure.latest_modified_at_ms)}`);
  if (disclosure.selected !== null) facts.push(`当前读第 ${disclosure.selected} 版`);
  if (disclosure.written_at_ms !== null) facts.push(`写于 ${formatStampMs(disclosure.written_at_ms)}`);
  if (facts.length === 0 && !disclosure.full_text) return null;
  return <div className="revision" data-revision-family={disclosure.family}>
    {facts.length ? <span>{facts.join(' · ')}</span> : null}
    {disclosure.content_digest ? <code>{disclosure.content_digest.slice(0, 12)}</code> : null}
    {disclosure.full_text ? <div className="revision-full">
      <h4>第 {disclosure.full_text.revision} 版全文{disclosure.full_text.written_at_ms !== null ? ` · 写于 ${formatStampMs(disclosure.full_text.written_at_ms)}` : ''}</h4>
      {disclosure.full_text.text !== null
        ? <p>{disclosure.full_text.text}</p>
        : <small>请求的版本全文未解析</small>}
    </div> : null}
    {disclosure.full_text_unreadable ? <small>请求的版本全文不可读</small> : null}
  </div>;
}

/** Store-level revision summary exactly as projected; no derived chain. */
function ContentRevisionDisclosureView({ revision }: { revision: ContentRevisionDisclosure | null }) {
  if (!revision) return null;
  return <div className="revision">
    <span>旧版本 {revision.old_version_count} 版 · 最新修改 {formatStampMs(revision.latest_modified_at_ms)}</span>
    <code>{revision.latest_digest.slice(0, 12)}</code>
  </div>;
}

/** Compiler facts exactly as the build recorded them; nothing is inferred. */
function compilerLabel(compiler: BuildManifest['compiler'] | undefined): string {
  if (!compiler) return '构建 manifest 未记录 compiler。';
  const runtime = compiler.runtime_imported === true ? '浏览器运行期导入编译器' : '浏览器未导入编译器';
  if (compiler.status !== 'build_time_compiled') return `${compiler.status}；${runtime}。`;
  const names = (compiler.prototypes ?? []).map((prototype) => prototype.name);
  const profile = compiler.profile ?? 'profile 未记录';
  if (names.length === 0) return `构建期编译，未记录已编译 prototype（${profile}）；${runtime}。`;
  return `构建期编译 ${names.length} 个 Shenko prototype（${names.join('、')}，${profile}）；${runtime}。`;
}

function Fact({ label, value, mono = false }: { label: string; value: string; mono?: boolean }) {
  return <div className="fact"><dt>{label}</dt><dd className={mono ? 'is-mono' : undefined} title={value}>{value}</dd></div>;
}

function wsUrl(): string {
  if (!location.host) return '不可用';
  return `${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/ws`;
}

export function ConfigSurface({ node, projection, emit }: { node?: UiNode; projection: UiProjection; emit: Emit }) {
  if (!node) return null;
  const config = node.props.config;
  const self = node.props.self;
  const manifest = node.props.manifest as BuildManifest | undefined;
  const unknown = (value: unknown) => value === undefined || value === null || value === '' || value === 'unknown' ? '未知' : String(value);
  const caps = projection.capabilities;
  return (
    <Page title="配置" lead="来自当前服务接口与本地构建记录的运行事实。未知就显示未知。" hidden={node.props.hidden} className="config">
      <Group title="运行">
        <dl className="facts">
          <Fact label="模型绑定" value={node.props.config_error ? '接口不可用' : unknown(config?.model)} />
          <Fact label="主体状态" value={unknown(self?.status)} />
          <Fact label="传输" value={connectionAnnouncement(node.props.connection)} />
          <Fact label="最近已见提交" value={node.props.step ? `第 ${node.props.step} 次` : '尚无'} />
          <Fact label="Runtime generation" value={unknown(config?.generation)} mono />
          <Fact label="API key" value={typeof config?.api_key_configured === 'boolean' ? config.api_key_configured ? '已配置（未读取值）' : '未配置' : '未知'} />
          <Fact label="服务版本" value={unknown(node.props.server_version)} mono />
          <Fact label="数据目录" value={unknown(config?.data_dir)} mono />
          <Fact label="WebSocket" value={wsUrl()} mono />
          <Fact label="Subject ID" value={projection.subject_id ?? '未知'} mono />
        </dl>
      </Group>
      <Group title="配置项">
        <ConfigEntries node={node} emit={emit} />
      </Group>
      <Group title="输入与时间线能力">
        <dl className="facts">
          <Fact label="追加" value={caps.append ? caps.durable_input_receipts ? '可用 · 持久回执' : '可用 · 传输回执' : '不可用'} />
          <Fact label="引导（steer）" value={caps.steer ? '可用 · 下一边界处置' : '未提供'} />
          <Fact label="接纳 / 选入回执" value={caps.durable_input_receipts ? '由 Store 回执' : '不可用'} />
          <Fact label="流式输出" value={caps.streaming ? '生产者已接线 · 仅已持久 chunk' : '无生产者 · 只显示已提交内容'} />
          <Fact label="已提交回放" value={caps.committed_replay ? '已接入' : '未知'} />
        </dl>
      </Group>
      <Group title="前端构建">
        {manifest ? <dl className="facts">
          <Fact label="Proto-UI" value={manifest.upstream.commit} mono />
          <Fact label="包" value={manifest.packages.join(' · ')} mono />
          <Fact label="采用的 family" value={manifest.adopted_families.join('、')} />
          <Fact label="Compiler" value={compilerLabel(manifest.compiler)} />
        </dl> : <Note text="构建 manifest 未读取。" />}
        <p className="links">
          <a href="/assets/licenses/PROTO-UI-LICENSE" target="_blank" rel="noreferrer">Proto-UI 许可</a>
          <a href="/assets/licenses/SHADCN-NOTICES.md" target="_blank" rel="noreferrer">shadcn 来源与许可</a>
          <a href="/assets/licenses/REACT-LICENSE" target="_blank" rel="noreferrer">React 许可</a>
        </p>
      </Group>
    </Page>
  );
}

export function PlaygroundSurface({ node }: { node?: UiNode }) {
  if (!node) return null;
  const playground: PlaygroundView | undefined = node.props.playground;
  const roots = Array.isArray(playground?.roots) ? playground.roots : [];
  const artifacts = Array.isArray(playground?.artifacts) ? playground.artifacts : [];
  const namespaces = Array.isArray(playground?.namespaces) ? playground.namespaces : [];
  return (
    <Page title="Playground" lead="主体长期保留的作品，以及它们的根与配置命名空间。" hidden={node.props.hidden}>
      {node.props.error
        ? <Note tone="error" text="Playground 接口不可用。" />
        : playground?.available === false
          ? <Note text="Playground 需要持久 Store 与主体。" />
          : <>
            <Group title="作品">
              {artifacts.length === 0 ? <Note text="还没有作品。" /> : (
                <ul className="rows">{artifacts.map((artifact) => (
                  <li className="row" key={String(artifact.artifact_id)}>
                    <Icon name="file" />
                    <span className="row-main">
                      <span className="row-title">{String(artifact.title ?? artifact.artifact_id)}</span>
                      <span className="row-sub">
                        r{String(artifact.revision ?? '—')} · {String(artifact.mime ?? '—')} · {artifact.backend ? '含后端' : '内容'}
                      </span>
                    </span>
                    <span className="row-state">{String(artifact.lifecycle ?? 'draft')}</span>
                    <time className="row-time">{formatStamp(artifact.updated_at)}</time>
                  </li>
                ))}</ul>
              )}
            </Group>
            <Group title="可选根">
              {roots.length === 0 ? <Note text="尚无已选根。" /> : (
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
            <Group title="配置命名空间">
              {namespaces.length === 0 ? <Note text="尚未注册配置命名空间。" /> : (
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

const SECTION_LABELS: Record<string, string> = { status: '当前状态', activity: '近期提交', artifacts: '产物', devices: '设备' };

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
      a11yLabel="主体详情"
      onOpenChange={(detail: { open: boolean }) => {
        if (detail.open !== open) emit('context', 'panel.toggle');
      }}
    >
      <DrawerMask className="sheet-mask" />
      <DrawerContent side="right" className="sheet">
        <DrawerHeader className="sheet-head">
          <div className="sheet-title">
            <DrawerTitle className="sheet-name">{name || '未命名主体'}</DrawerTitle>
            <DrawerDescription className="sheet-sub">
              {projection.identity?.revision ? `身份第 ${projection.identity.revision} 版` : '身份版本未知'}
            </DrawerDescription>
          </div>
          <Button variant="ghost" size="icon" label="关闭详情" onClick={() => emit('context', 'panel.toggle')}>
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

/// The instance's own name and who owns it (docs/specs/web-interface.md#spec-21-1, docs/specs/plugins.md#spec-44-4).
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
      <h3>这个实例</h3>
      <div className="field">
        <label className="field-label" id="identity-name-label">名字</label>
        <div className="field-row">
          <ProtoInput
            className="input"
            labelledBy="identity-name-label"
            value={nameDraft}
            placeholder="留空则让它自己取名"
            onValueChange={(detail: { value: string }) => emit(node.node_id, node.props.identity_name_draft_ref ?? '', { value: detail.value })}
          />
          <Button variant="outline" size="sm" onClick={() => nameRef && emit(node.node_id, nameRef)}>取名</Button>
        </div>
      </div>
      {owner?.available === false ? (
        <Note text="这台安装还没有装上归属机制。" />
      ) : (
        <div className="field">
          <span className="field-label" id="owner-claim-label">
            归属 · {owner?.claimed === true ? '已归你' : owner?.claim_pending === true ? '认领待完成' : '尚未认领'}
          </span>
          {!owner?.claimed && (
            <div className="field-row">
              <ProtoInput
                className="input"
                labelledBy="owner-claim-label"
                value={claimDraft}
                placeholder="这台安装给出的认领凭据"
                onValueChange={(detail: { value: string }) => emit(node.node_id, node.props.owner_claim_draft_ref ?? '', { value: detail.value })}
              />
              <Button variant="outline" size="sm" onClick={() => claimRef && emit(node.node_id, claimRef)}>认领</Button>
            </div>
          )}
        </div>
      )}
    </section>
  );
}

/// The work the reader opened, with the actions bound to its revision
/// (docs/specs/configuration-playground.md#spec-57-6). Every action names the revision the reader saw: a work
/// that moved on refuses the stale action instead of quietly applying to
/// whatever is there now.
function OpenedWorkPanel({ node, emit }: { node: UiNode; emit: Emit }) {
  const work = node.props.work as OpenedWork | undefined;
  const refs = (node.props.work_action_refs ?? []) as string[];
  if (node.props.work_error === true) {
    return <section className="sheet-section"><Note tone="error" text="这件作品现在读不到。" /></section>;
  }
  if (!work) return null;
  return (
    <section className="sheet-section work" data-artifact-id={work.artifact_id}>
      <h3>{String(work.title ?? work.artifact_id)}</h3>
      <p className="sheet-sub">r{String(work.revision ?? '—')} · {String(work.lifecycle ?? 'draft')} · {String(work.mime ?? '—')}</p>
      {work.content !== undefined && <pre className="work-content" tabIndex={0}><code>{work.content}</code></pre>}
      <div className="button-row">
        <Button variant="outline" size="sm" onClick={() => refs[0] && emit(node.node_id, refs[0])}>暂存为候选</Button>
        <Button variant="outline" size="sm" onClick={() => refs[1] && emit(node.node_id, refs[1])}>采纳候选</Button>
        <Button variant="ghost" size="sm" onClick={() => refs[2] && emit(node.node_id, refs[2])}>回到旧修订</Button>
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
    <Fact label="主体状态" value={self?.status && self.status !== 'unknown' ? self.status : '未知'} />
    <Fact label="传输" value={connectionAnnouncement(node.props.connection)} />
    <Fact label="最近已见提交" value={node.props.step ? `第 ${node.props.step} 次` : '尚无'} />
    <Fact label="已连接客户端" value={typeof self?.connected_clients === 'number' ? String(self.connected_clients) : '未知'} />
    <Fact label="模型绑定" value={config?.model && config.model !== 'unknown' ? config.model : '未知'} />
  </dl>;
  if (name === 'activity') {
    const parts = (node.props.timeline as TimelinePart[]).filter((part) => part.content).slice(-3).reverse();
    return parts.length
      ? <ul className="mini-rows">{parts.map((part) => <li key={part.part_id}>
        <span className="mini-meta">第 {part.step} 轮 · {formatTime(part.observed_at)}</span>
        <span className="mini-text">{firstLine(part.content)}</span>
      </li>)}</ul>
      : <p className="muted">尚无已提交内容。</p>;
  }
  if (name === 'artifacts') {
    const artifacts = Array.isArray(self?.artifacts) ? self.artifacts : [];
    return artifacts.length
      ? <ul className="mini-rows">{artifacts.map((artifact: { id: string; title?: string; kind?: string }) => <li key={artifact.id}>
        <span className="mini-text">{artifact.title || artifact.id}</span>
        <span className="mini-meta">{artifact.kind || '未知类型'}</span>
      </li>)}</ul>
      : <p className="muted">暂无产物。</p>;
  }
  const devices = Array.isArray(self?.devices) ? self.devices : [];
  return devices.length
    ? <ul className="mini-rows">{devices.map((device: { id: string; name?: string; label?: string; kind?: string; online?: boolean }) => <li key={device.id}>
      <span className="mini-text">{device.name || device.label || device.id}</span>
      <span className="mini-meta">{device.online === true ? '在线' : device.online === false ? '离线' : '状态未知'}{device.kind ? ` · ${device.kind}` : ''}</span>
    </li>)}</ul>
    : <p className="muted">设备发现尚未接入。</p>;
}
