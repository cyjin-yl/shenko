import * as React from 'react';
import { text as t } from './messages';
import { Icon } from './icons';
import {
  Button, ResizableRoot, ResizablePanel, ResizableHandle, type InspectorResizeHandle,
} from './ui-kit';
import { storageGet, storageSet } from './ui-preferences';
import {
  INSPECTOR_DESKTOP_MIN, INSPECTOR_WIDTH_KEY, inspectorLayout, inspectorProposal,
  readInspectorWidth, type InspectorMeasurement,
} from './inspector-sizing';

const CLOSED_SIZES = [100, 0] as const;
const INITIAL_SIZES = [58, 42] as const;
const INITIAL_CONSTRAINTS = [
  { min: 0, max: 100 },
  { min: 0, max: 100, collapsible: true, collapsedSize: 0 },
] as const;
const DESKTOP_QUERY = `(min-width: ${INSPECTOR_DESKTOP_MIN}px)`;
type SizeChange = { sizes: readonly number[] };

/**
 * One real shell boundary: the stream and its optional lens.
 * Resizable owns input, proportions, keyboard policy and contact retirement.
 * This component owns only pixel constraints, browser preference and which
 * lens is open. Narrow layout retains the same stream and lens DOM.
 */
export function InspectorLayout({ openId, title, subtitle, inspector, onClose, children }: {
  openId: string | null;
  title: string;
  subtitle?: string;
  inspector: React.ReactNode;
  onClose: () => void;
  children: React.ReactNode;
}) {
  const container = React.useRef<HTMLDivElement>(null);
  const owner = React.useRef<InspectorResizeHandle>(null);
  const lastFocused = React.useRef<HTMLElement | null>(null);
  const opener = React.useRef<HTMLElement | null>(null);
  const focusAfterChange = React.useRef<'separator' | 'opener' | 'lens' | null>(null);
  const [wide, setWide] = React.useState(() => window.matchMedia(DESKTOP_QUERY).matches);
  const [measurement, setMeasurement] = React.useState<InspectorMeasurement | null>(null);
  const [preferredWidth, setPreferredWidth] = React.useState(() =>
    readInspectorWidth(storageGet(INSPECTOR_WIDTH_KEY)));
  const [isCollapsed, setCollapsed] = React.useState(false);
  const collapsed = wide && openId !== null && isCollapsed;
  const layout = measurement ? inspectorLayout(measurement, preferredWidth) : null;
  const enabled = wide && openId !== null && layout !== null;
  const lease = React.useMemo(() => ({ openId, wide, extent: layout?.extent }),
    [openId, wide, layout?.extent]);
  const liveLease = React.useRef<typeof lease | null>(null);

  React.useLayoutEffect(() => {
    liveLease.current = lease;
    return () => { if (liveLease.current === lease) liveLease.current = null; };
  }, [lease]);

  // A new opening restores the last expanded width before paint; the stream
  // remains mounted. Durable reading state never lives in a memo cache.
  React.useLayoutEffect(() => { setCollapsed(false); }, [openId]);

  React.useEffect(() => {
    const query = window.matchMedia(DESKTOP_QUERY);
    const update = () => {
      const handle = container.current?.querySelector('.inspector-resize-handle');
      if (!query.matches && document.activeElement === handle) focusAfterChange.current = 'lens';
      setWide(query.matches);
    };
    query.addEventListener('change', update);
    update();
    return () => query.removeEventListener('change', update);
  }, []);

  React.useLayoutEffect(() => {
    const body = container.current;
    if (!body) return;
    let group: HTMLElement | null = null;
    let separator: HTMLElement | null = null;
    let current = true;
    const publish = (groupContentWidth: number, separatorWidth: number) => {
      if (!current) return;
      setMeasurement((previous) => previous?.groupContentWidth === groupContentWidth
        && previous.separatorWidth === separatorWidth
        ? previous : { groupContentWidth, separatorWidth });
    };
    // The public host classes identify only the two layout boxes we own.
    // Initial measurement occurs before paint; RO then tracks real reflow.
    const read = () => {
      if (!current || !group || !separator || !body.contains(group) || !group.contains(separator)) return;
      const style = getComputedStyle(group);
      const contentWidth = group.getBoundingClientRect().width
        - parseFloat(style.borderLeftWidth) - parseFloat(style.borderRightWidth)
        - parseFloat(style.paddingLeft) - parseFloat(style.paddingRight);
      publish(contentWidth, separator.getBoundingClientRect().width);
    };
    const observer = new ResizeObserver(read);
    const bind = () => {
      if (!current) return;
      // Stream/content mutations need no work once these exact public hosts
      // are live. Rebind only after their real publication or replacement.
      if (group && separator && body.contains(group) && group.contains(separator)) return;
      observer.disconnect();
      group = body.querySelector<HTMLElement>('.inspector-resize-group');
      separator = group?.querySelector<HTMLElement>('.inspector-resize-handle') ?? null;
      if (group && separator) {
        observer.observe(group);
        observer.observe(separator);
        read();
      } else {
        setMeasurement(null);
      }
    };
    const publication = new MutationObserver(bind);
    publication.observe(body, { childList: true, subtree: true });
    bind();
    return () => { current = false; observer.disconnect(); publication.disconnect(); };
  }, [openId, wide]);

  React.useLayoutEffect(() => {
    if (openId !== null) {
      const active = document.activeElement;
      if (active instanceof HTMLElement && active !== document.body
        && !container.current?.querySelector('.lens')?.contains(active)
        && !active.classList.contains('inspector-resize-handle')) opener.current = active;
      // The keyed public Handle retires the old input target on lens changes.
      // If that was the focused object, preserve focus on its replacement.
      if (wide && lastFocused.current?.classList.contains('inspector-resize-handle')
        && !lastFocused.current.isConnected && document.activeElement === document.body) {
        focusAfterChange.current = 'separator';
      }
    }
  }, [openId, wide]);

  React.useLayoutEffect(() => {
    const requested = focusAfterChange.current;
    if (!requested) return;
    const visible = (element: HTMLElement | null | undefined): element is HTMLElement =>
      !!element?.isConnected && element.getClientRects().length > 0
      && getComputedStyle(element).visibility !== 'hidden' && !element.closest('[inert]');
    // A desktop opener can become hidden when the same lens crosses the phone
    // breakpoint. Return to the shell's current visible destination instead of
    // leaving focus on the now-hidden lens or the document body.
    const shell = container.current?.closest('.shell');
    const returnTarget = [
      opener.current,
      shell?.querySelector<HTMLElement>('.tabbar-item[aria-current="page"]'),
      shell?.querySelector<HTMLElement>('.topbar .tab-item[data-active]'),
    ].find(visible);
    const target = requested === 'opener'
      ? (openId === null ? returnTarget : null)
      : requested === 'lens'
        ? (!wide && openId !== null
          ? container.current?.querySelector<HTMLElement>('.inspector-close') : null)
        : (wide && openId !== null
          ? container.current?.querySelector<HTMLElement>('.inspector-resize-handle') : null);
    if (visible(target)) {
      target.focus({ preventScroll: true });
      if (document.activeElement === target) focusAfterChange.current = null;
    }
  }, [openId, wide, collapsed, measurement]);

  const change = (detail: SizeChange) => {
    if (liveLease.current !== lease || !enabled || !layout) return;
    const proposal = inspectorProposal(detail.sizes, layout);
    if (!proposal) return;
    setCollapsed(proposal.collapsed);
    if (proposal.width !== null) setPreferredWidth(proposal.width);
  };
  const commit = (detail: SizeChange) => {
    if (liveLease.current !== lease || !enabled || !layout) return;
    const proposal = inspectorProposal(detail.sizes, layout);
    if (proposal?.width !== null && proposal?.width !== undefined) {
      storageSet(INSPECTOR_WIDTH_KEY, String(proposal.width));
    }
  };
  const request = (sizes: readonly number[]) => {
    if (liveLease.current !== lease || !enabled || !owner.current) return;
    const instance = owner.current;
    let accepted = false;
    focusAfterChange.current = 'separator';
    const invoke = () => { accepted = instance.getExposes().requestSizes(sizes, true); };
    if (instance.invokeInCallbackScope) instance.invokeInCallbackScope(invoke);
    else invoke();
    if (!accepted) focusAfterChange.current = null;
  };
  const close = () => {
    focusAfterChange.current = 'opener';
    onClose();
  };
  const sizes = !wide || openId === null || collapsed ? CLOSED_SIZES : layout?.sizes ?? INITIAL_SIZES;

  return (
    <div
      className="body"
      ref={container}
      data-inspector-layout={wide ? 'split' : 'overlay'}
      data-inspector-state={openId === null ? 'closed' : collapsed ? 'collapsed' : 'open'}
      onFocusCapture={(event) => {
        if (event.target instanceof HTMLElement) lastFocused.current = event.target;
      }}
    >
      <ResizableRoot
        ref={owner}
        hostClassName="inspector-resize-group"
        styleProfile="rhea"
        orientation="horizontal"
        sizes={sizes}
        constraints={wide ? layout?.constraints ?? INITIAL_CONSTRAINTS : INITIAL_CONSTRAINTS}
        disabled={!enabled}
        onSizesChange={change}
        onSizesCommit={commit}
      >
        <ResizablePanel index={0} hostClassName="inspector-stream-pane">{children}</ResizablePanel>
        <ResizableHandle key={openId ?? 'closed'} index={0} withHandle hostClassName="inspector-resize-handle" />
        <ResizablePanel index={1} hostClassName="inspector-lens-pane">
          <aside className="lens" hidden={openId === null || collapsed} aria-label={openId === null ? undefined : title}>
            <div className="lens-head">
              <div className="lens-head-id">
                <span className="lens-title">{title}</span>
                {subtitle && <span className="lens-sub">{subtitle}</span>}
              </div>
              <div className="lens-head-actions">
                <Button className="inspector-collapse" variant="ghost" size="icon"
                  label={t('shell.collapseLens')} disabled={!enabled}
                  onClick={() => request(CLOSED_SIZES)}><Icon name="panel" /></Button>
                <Button className="inspector-close" variant="ghost" size="icon"
                  label={t('shell.closeLens')} onClick={close}><Icon name="close" /></Button>
              </div>
            </div>
            <div className="lens-body">{inspector}</div>
          </aside>
        </ResizablePanel>
      </ResizableRoot>
      {collapsed && (
        <Button className="inspector-restore" variant="outline" size="sm"
          onClick={() => { if (layout) request(layout.sizes); }}>
          <Icon name="panel" />{t('shell.restoreLens')}
        </Button>
      )}
    </div>
  );
}
