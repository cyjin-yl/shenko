/**
 * Shenko's own line icons: one 20px grid, one stroke weight, drawn locally so
 * the shell needs no icon font or CDN (docs/specs/web-interface.md#spec-21-3).
 */
export type IconName =
  | 'timeline' | 'tasks' | 'memory' | 'config' | 'playground'
  | 'panel' | 'sun' | 'moon' | 'copy' | 'check' | 'send' | 'steer'
  | 'down' | 'close' | 'chevron' | 'more' | 'undo' | 'retry'
  | 'file' | 'open' | 'spark' | 'wrap' | 'info' | 'warning' | 'empty' | 'chat';

export function Icon({ name, size = 18 }: { name: IconName | string; size?: number }) {
  const common = {
    width: size,
    height: size,
    viewBox: '0 0 20 20',
    fill: 'none',
    stroke: 'currentColor',
    strokeWidth: 1.5,
    strokeLinecap: 'round' as const,
    strokeLinejoin: 'round' as const,
    'aria-hidden': true,
    focusable: false,
  };
  switch (name) {
    case 'timeline': return <svg {...common}><path d="M4 5h12M4 10h8M4 15h10" /></svg>;
    case 'tasks': return <svg {...common}><rect x="3.5" y="3.5" width="13" height="13" rx="3.5" /><path d="m7 10 2 2 4-4.5" /></svg>;
    case 'memory': return <svg {...common}><path d="M10 16.5c-3.6 0-6.5-2.6-6.5-6.2C3.5 6.6 6.4 3.5 10 3.5s6.5 3.1 6.5 6.8c0 3.6-2.9 6.2-6.5 6.2Z" /><path d="M10 6.5v4l2.5 1.5" /></svg>;
    case 'config': return <svg {...common}><path d="M4 6h7M15 6h1M4 14h1M9 14h7" /><circle cx="13" cy="6" r="2" /><circle cx="7" cy="14" r="2" /></svg>;
    case 'playground': return <svg {...common}><rect x="3.5" y="4" width="13" height="12" rx="3" /><path d="M3.5 8h13M6.5 6h.01M8.5 6h.01" /></svg>;
    case 'panel': return <svg {...common}><rect x="3" y="4" width="14" height="12" rx="3" /><path d="M12 4v12" /></svg>;
    case 'sun': return <svg {...common}><circle cx="10" cy="10" r="3" /><path d="M10 2.5v1.5M10 16v1.5M2.5 10H4M16 10h1.5M4.7 4.7l1 1M14.3 14.3l1 1M15.3 4.7l-1 1M5.7 14.3l-1 1" /></svg>;
    case 'moon': return <svg {...common}><path d="M16 12.2A6.5 6.5 0 0 1 7.8 4a6.5 6.5 0 1 0 8.2 8.2Z" /></svg>;
    case 'copy': return <svg {...common}><rect x="7" y="7" width="9.5" height="9.5" rx="2.5" /><path d="M13 7V5.5a2 2 0 0 0-2-2H5.5a2 2 0 0 0-2 2V11a2 2 0 0 0 2 2H7" /></svg>;
    case 'check': return <svg {...common}><path d="m5 10.5 3 3 7-7.5" /></svg>;
    case 'send': return <svg {...common}><path d="M10 16V4.5M5 9l5-5 5 5" /></svg>;
    case 'steer': return <svg {...common}><path d="M4 15c0-5 3-8 8-8h4" /><path d="m13 4 3 3-3 3" /></svg>;
    case 'down': return <svg {...common}><path d="M10 4v11.5M5 11l5 5 5-5" /></svg>;
    case 'close': return <svg {...common}><path d="m5.5 5.5 9 9M14.5 5.5l-9 9" /></svg>;
    case 'chevron': return <svg {...common}><path d="m7.5 5 5 5-5 5" /></svg>;
    case 'more': return <svg {...common}><path d="M5 10h.01M10 10h.01M15 10h.01" strokeWidth={2.4} /></svg>;
    case 'undo': return <svg {...common}><path d="M7.5 5.5 4 9l3.5 3.5" /><path d="M4.5 9H12a4 4 0 0 1 0 8h-2" /></svg>;
    case 'retry': return <svg {...common}><path d="M15.5 10a5.5 5.5 0 1 1-1.8-4.1" /><path d="M15.5 3.5v3.5H12" /></svg>;
    case 'file': return <svg {...common}><path d="M11.5 3H6.5a2 2 0 0 0-2 2v10a2 2 0 0 0 2 2h7a2 2 0 0 0 2-2V7Z" /><path d="M11.5 3v4h4" /></svg>;
    case 'open': return <svg {...common}><path d="M8 4.5H5.5a2 2 0 0 0-2 2v8a2 2 0 0 0 2 2h8a2 2 0 0 0 2-2V12" /><path d="M11 3.5h5.5V9M16.5 3.5 9 11" /></svg>;
    case 'spark': return <svg {...common}><path d="M10 3v3M10 14v3M3 10h3M14 10h3M5.5 5.5l1.8 1.8M12.7 12.7l1.8 1.8M14.5 5.5l-1.8 1.8M7.3 12.7l-1.8 1.8" /></svg>;
    case 'wrap': return <svg {...common}><path d="M3.5 5h13M3.5 10h10.5a2.5 2.5 0 0 1 0 5H10" /><path d="m11.5 13.2-1.8 1.8 1.8 1.8M3.5 15h3" /></svg>;
    case 'info': return <svg {...common}><circle cx="10" cy="10" r="7" /><path d="M10 9v4.5M10 6.5h.01" /></svg>;
    case 'warning': return <svg {...common}><path d="M10 3.5 17 16H3Z" /><path d="M10 8.5v3.5M10 14h.01" /></svg>;
    case 'chat': return <svg {...common}><path d="M10 3.5c3.9 0 7 2.6 7 5.8s-3.1 5.8-7 5.8c-.8 0-1.6-.1-2.3-.3L4.2 16.3l.9-2.9C3.8 12.3 3 10.9 3 9.3 3 6.1 6.1 3.5 10 3.5Z" /></svg>;
    case 'empty': return <svg {...common}><rect x="3.5" y="5" width="13" height="10" rx="3" strokeDasharray="2.5 2.5" /></svg>;
    default: return <svg {...common}><circle cx="10" cy="10" r="6" /></svg>;
  }
}
