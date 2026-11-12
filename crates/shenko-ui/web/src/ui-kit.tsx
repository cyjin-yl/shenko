import * as React from 'react';
import { createPortal } from 'react-dom';
import { createReactAdapter } from '@proto.ui/adapter-react';
import { shadcnButton } from '@proto.ui/prototypes-shadcn/button';
import { shadcnTextareaRoot } from '@proto.ui/prototypes-shadcn/textarea';
import {
  shadcnScrollAreaRoot,
  shadcnScrollAreaViewport,
  shadcnScrollAreaScrollbar,
  shadcnScrollAreaThumb,
} from '@proto.ui/prototypes-shadcn/scroll-area';
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
  if (demo) return <CompiledTransportBadge label="本地模拟" tone="local" />;
  // The generated component re-applies its tone on update but keeps the
  // first text it rendered, so a changed label remounts it.
  const label = connectionLabel(connection);
  return <CompiledTransportBadge key={label} label={label} tone={connection} />;
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
    case 'online': return '已连接';
    case 'connecting': return '连接中';
    case 'reconnecting': return '正在重连';
    default: return '离线';
  }
}

export function connectionAnnouncement(connection: ConnectionState): string {
  return connection === 'online' ? '传输已连接' : connection === 'reconnecting' ? '连接中断，正在重连' : '传输未连接';
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
  if (value === null) return '时间未知';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return '时间未知';
  const now = new Date();
  const sameDay = (left: Date, right: Date) => left.getFullYear() === right.getFullYear()
    && left.getMonth() === right.getMonth() && left.getDate() === right.getDate();
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  const weekdays = '日一二三四五六';
  const base = `${date.getMonth() + 1}月${date.getDate()}日 周${weekdays[date.getDay()]}`;
  if (sameDay(date, now)) return `今天 · ${base}`;
  if (sameDay(date, yesterday)) return `昨天 · ${base}`;
  return date.getFullYear() === now.getFullYear() ? base : `${date.getFullYear()}年${base}`;
}

export function formatStamp(value: unknown): string {
  if (typeof value !== 'string' || !value) return '—';
  const date = new Date(value);
  if (Number.isNaN(date.getTime())) return value;
  return `${date.getMonth() + 1}月${date.getDate()}日 ${formatTime(value)}`;
}

/** Disclosure timestamps are epoch milliseconds; only presentation changes here. */
export function formatStampMs(value: number): string {
  return Number.isFinite(value) ? formatStamp(new Date(value).toISOString()) : '—';
}

export function firstLine(text: string): string {
  return text.split('\n').find((line) => line.trim())?.trim() ?? '（空）';
}
