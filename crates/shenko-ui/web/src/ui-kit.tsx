import { text as t, preferredLocale } from './messages';
import * as React from 'react';
import { createPortal } from 'react-dom';
import { createReactAdapter, type ReactAdapterHandle } from '@proto.ui/adapter-react';
import { defineAsHook, definePrototype } from '@proto.ui/core';
import { asScrollSurface } from '@proto.ui/hooks';
import { shadcnButton } from '@proto.ui/prototypes-shadcn/button';
import { shadcnTextareaRoot } from '@proto.ui/prototypes-shadcn/textarea';
import {
  shadcnScrollAreaRoot,
  shadcnScrollAreaViewport,
  shadcnScrollAreaScrollbar,
  shadcnScrollAreaThumb,
} from '@proto.ui/prototypes-shadcn/scroll-area';
import {
  shadcnMessageScrollerRoot,
  shadcnMessageScrollerViewport,
  type ShadcnMessageScrollerViewportProps,
  type MessageScrollerViewportExposes,
} from '@proto.ui/prototypes-shadcn/message-scroller';
import {
  shadcnCollapsibleRoot,
  shadcnCollapsibleTrigger,
  shadcnCollapsibleContent,
} from '@proto.ui/prototypes-shadcn/collapsible';
import {
  drawerRoot,
  drawerContent,
  drawerMask,
  drawerHeader,
  drawerTitle,
  drawerDescription,
} from '@proto.ui/prototypes-shadcn/drawer';
import { shadcnToggleGroupRoot, shadcnToggleGroupItem } from '@proto.ui/prototypes-shadcn/toggle-group';
import { shadcnInputRoot } from '@proto.ui/prototypes-shadcn/input';
import { shadcnSeparatorRoot } from '@proto.ui/prototypes-shadcn/separator';
import {
  shadcnResizableRoot,
  shadcnResizablePanel,
  shadcnResizableHandle,
} from '@proto.ui/prototypes-shadcn/resizable';
import { shadcnToggle } from '@proto.ui/prototypes-shadcn/toggle';
import type { ConnectionState } from './projection';
import { CompiledTransportBadge } from '../generated/prototypes/transport-badge/Component';

// The actual upstream prototypes and official runtime adapter, not local
// lookalikes. The Proto-UI compiler is not part of this graph: it runs at
// build time (web/compiler.mjs) and this bundle only imports the React DOM
// source it emitted for Shenko's own Prototypes.
const adapt = createReactAdapter({ ...React, createPortal });
export const ProtoButton = adapt(shadcnButton);
export const ProtoTextarea = adapt(shadcnTextareaRoot);
export const ScrollRoot = adapt(shadcnScrollAreaRoot);
export const ScrollViewport = adapt(shadcnScrollAreaViewport);
export const ScrollScrollbar = adapt(shadcnScrollAreaScrollbar);
export const ScrollThumb = adapt(shadcnScrollAreaThumb);
export const MessageRoot = adapt(shadcnMessageScrollerRoot);
// Integration boundary: Proto-UI owns follow/depart/resume and its
// lease. Shenko owns persisted entry keys and selection offsets. Turn off the
// generic visible anchor so there is exactly one anchor owner, including
// backfill and the late mounting of compiled message content.
const asMessageViewport = defineAsHook(shadcnMessageScrollerViewport);
const readingViewport = definePrototype<ShadcnMessageScrollerViewportProps, MessageScrollerViewportExposes>({
  name: 'shenko-reading-viewport',
  modules: asMessageViewport.modules,
  setup() {
    const inherited = asMessageViewport();
    asScrollSurface().configure({ anchor: { mode: 'off' } });
    return inherited.render;
  },
});
export type ReadingViewportHandle = ReactAdapterHandle<typeof readingViewport>;
const AdaptedReadingViewport = adapt(readingViewport);
// The Adapter publishes its imperative ref before its own setup layout
// effect. Publish the bridge ref in the parent layout effect, after that
// child has installed its public exposes, including on StrictMode replay.
export const ReadingViewport = React.forwardRef<ReadingViewportHandle, React.ComponentPropsWithoutRef<typeof AdaptedReadingViewport>>(
  function ReadingViewport(props, ref) {
    const child = React.useRef<ReadingViewportHandle>(null);
    React.useImperativeHandle(ref, () => child.current!, []);
    return <AdaptedReadingViewport {...props} ref={child} />;
  },
);
export const CollapsibleRoot = adapt(shadcnCollapsibleRoot);
export const CollapsibleTrigger = adapt(shadcnCollapsibleTrigger);
export const CollapsibleContent = adapt(shadcnCollapsibleContent);
export const DrawerRoot = adapt(drawerRoot);
export const DrawerMask = adapt(drawerMask);
export const DrawerContent = adapt(drawerContent);
export const DrawerHeader = adapt(drawerHeader);
export const DrawerTitle = adapt(drawerTitle);
export const DrawerDescription = adapt(drawerDescription);
export const ToggleGroupRoot = adapt(shadcnToggleGroupRoot);
export const ToggleGroupItem = adapt(shadcnToggleGroupItem);
export const ProtoInput = adapt(shadcnInputRoot);
export const Separator = adapt(shadcnSeparatorRoot);
export const ProtoToggle = adapt(shadcnToggle);
export const ResizableRoot = adapt(shadcnResizableRoot);
export const ResizablePanel = adapt(shadcnResizablePanel);
export const ResizableHandle = adapt(shadcnResizableHandle);
export type InspectorResizeHandle = ReactAdapterHandle<typeof shadcnResizableRoot>;

export type Emit = (nodeId: string, actionId: string, payload?: Record<string, unknown>) => void;

type ButtonVariant = 'default' | 'secondary' | 'outline' | 'ghost';
type ButtonSize = 'default' | 'sm' | 'icon';

/** One button vocabulary for the whole shell, always the upstream shadcn family. */
export function Button({
  variant = 'ghost',
  size = 'sm',
  className,
  disabled,
  onClick,
  label,
  children,
}: {
  variant?: ButtonVariant;
  size?: ButtonSize;
  className?: string;
  disabled?: boolean;
  onClick?: () => void;
  /** Accessible name for icon-only buttons, read as visually hidden text. */
  label?: string;
  children: React.ReactNode;
}) {
  return (
    <ProtoButton
      styleProfile="rhea"
      variant={variant}
      size={size}
      className={['sk-button', `sk-button-${variant}`, `sk-button-${size}`, className].filter(Boolean).join(' ')}
      disabled={disabled}
      onClick={onClick}
    >
      {children}
      {label && <span className="sr-only">{label}</span>}
    </ProtoButton>
  );
}

// The chip is Shenko's own Prototype, compiled at build time into React DOM
// source by the pinned Proto-UI compiler. This caller keeps the transport facts
// and decides only the label and tone; the Prototype renders markup and tokens.
export function TransportBadge({ connection, demo }: { connection: ConnectionState; demo: boolean }) {
  if (demo) return <CompiledTransportBadge label={t('transport.demo')} tone="local" />;
  return <CompiledTransportBadge label={connectionLabel(connection)} tone={connection} />;
}

/** A two-state control is the upstream toggle family, which owns aria-pressed. */
export function Toggle({ active, onChange, label, children }: {
  active: boolean; onChange: () => void; label: string; children: React.ReactNode;
}) {
  return (
    <ProtoToggle
      styleProfile="rhea"
      variant="default"
      size="sm"
      className="sk-toggle"
      active={active}
      onActiveChange={(detail: { active: boolean }) => { if (detail.active !== active) onChange(); }}
    >
      {children}
      <span className="sr-only">{label}</span>
    </ProtoToggle>
  );
}

export function connectionLabel(connection: ConnectionState): string {
  switch (connection) {
    case 'online': return t('transport.online');
    case 'connecting': return t('transport.connecting');
    case 'reconnecting': return t('transport.reconnecting');
    default: return t('transport.offline');
  }
}

export function connectionAnnouncement(connection: ConnectionState): string {
  return connection === 'online' ? t('transport.connectedAnnouncement') : connection === 'reconnecting' ? t('transport.reconnectAnnouncement') : t('transport.offlineAnnouncement');
}

export function Note({ text, tone = 'neutral', icon }: { text: string; tone?: 'neutral' | 'error'; icon?: React.ReactNode }) {
  return <div className={`note is-${tone}`} role={tone === 'error' ? 'alert' : undefined}>
    {icon}
    <p>{text}</p>
  </div>;
}

export function formatTime(value: unknown): string {
  if (typeof value !== 'string' || !value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '—';
  return `${String(date.getHours()).padStart(2, '0')}:${String(date.getMinutes()).padStart(2, '0')}`;
}

export function dayLabel(value: string | null): string {
  if (value === null) return t('time.unknown');
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return t('time.unknown');
  const now = new Date();
  const sameDay = (left: Date, right: Date) => left.getFullYear() === right.getFullYear()
    && left.getMonth() === right.getMonth() && left.getDate() === right.getDate();
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const base = new Intl.DateTimeFormat(preferredLocale(), {
    month: 'short', day: 'numeric', weekday: 'short',
    ...(date.getFullYear() === now.getFullYear() ? {} : { year: 'numeric' as const }),
  }).format(date);
  if (sameDay(date, now)) return t('time.today', { date: base });
  if (sameDay(date, yesterday)) return t('time.yesterday', { date: base });
  return base;
}

export function formatStamp(value: unknown): string {
  if (typeof value !== 'string' || !value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return `${new Intl.DateTimeFormat(preferredLocale(), { month: 'short', day: 'numeric' }).format(date)} ${formatTime(value)}`;
}

/** Disclosure timestamps are epoch milliseconds; only presentation changes here. */
export function formatStampMs(value: number): string {
  return Number.isFinite(value) ? formatStamp(new Date(value).toISOString()) : '—';
}

export function firstLine(text: string): string {
  return text.split('\n').find((line) => line.trim())?.trim() ?? t('common.empty');
}
