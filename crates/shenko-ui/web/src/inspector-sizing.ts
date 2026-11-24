import type { ResizablePanelConstraint } from '@proto.ui/prototypes-shadcn/resizable';

// Keep the shell's existing desktop breakpoint and clamp(400px, 42vw, 680px).
export const INSPECTOR_DESKTOP_MIN = 1100;
export const INSPECTOR_MIN_WIDTH = 400;
export const INSPECTOR_MAX_WIDTH = 680;
export const INSPECTOR_DEFAULT_FRACTION = 0.42;
export const INSPECTOR_WIDTH_KEY = 'shenko.inspector.width.v1';
// Before resizing, the narrowest desktop main column was 1100 * (1 - .42).
// Retain that 638px reading space when the reader widens the lens.
export const STREAM_MIN_WIDTH = INSPECTOR_DESKTOP_MIN * (1 - INSPECTOR_DEFAULT_FRACTION);

export interface InspectorMeasurement {
  groupContentWidth: number;
  separatorWidth: number;
}

export interface InspectorLayout {
  extent: number;
  width: number;
  maxWidth: number;
  sizes: readonly [number, number];
  constraints: readonly ResizablePanelConstraint[];
}

/** Browser preference only; unknown or corrupt values never become constraints. */
export function readInspectorWidth(value: string): number | null {
  if (!value.trim()) return null;
  const width = Number(value);
  return Number.isFinite(width) && width >= INSPECTOR_MIN_WIDTH && width <= INSPECTOR_MAX_WIDTH
    ? width : null;
}

/**
 * Application layout policy. The public owner still measures all pointer
 * deltas against the actual sum of Panel extents and owns cancellation.
 * The group has no padding/gap; its fixed separator is excluded explicitly.
 */
export function inspectorLayout(
  measurement: InspectorMeasurement,
  preferredWidth: number | null,
): InspectorLayout | null {
  const { groupContentWidth, separatorWidth } = measurement;
  if (!Number.isFinite(groupContentWidth) || !Number.isFinite(separatorWidth)
    || groupContentWidth < INSPECTOR_DESKTOP_MIN || separatorWidth <= 0) return null;
  const extent = groupContentWidth - separatorWidth;
  if (extent < STREAM_MIN_WIDTH + INSPECTOR_MIN_WIDTH) return null;
  const maxWidth = Math.min(INSPECTOR_MAX_WIDTH, extent - STREAM_MIN_WIDTH);
  const preferred = preferredWidth !== null && Number.isFinite(preferredWidth)
    ? preferredWidth : groupContentWidth * INSPECTOR_DEFAULT_FRACTION;
  const width = Math.min(maxWidth, Math.max(INSPECTOR_MIN_WIDTH, preferred));
  const end = width * 100 / extent;
  return {
    extent, width, maxWidth,
    sizes: [100 - end, end],
    constraints: [
      { min: STREAM_MIN_WIDTH * 100 / extent, max: 100 },
      { min: INSPECTOR_MIN_WIDTH * 100 / extent, max: maxWidth * 100 / extent,
        collapsible: true, collapsedSize: 0 },
    ],
  };
}

/** Accept complete public proposals without inventing a second resize solver. */
export function inspectorProposal(
  sizes: readonly number[],
  layout: InspectorLayout,
): { collapsed: boolean; width: number | null } | null {
  if (sizes.length !== 2 || !Object.hasOwn(sizes, 0) || !Object.hasOwn(sizes, 1)
    || !sizes.every(Number.isFinite) || sizes.some((value) => value < 0)
    || Math.abs(sizes[0] + sizes[1] - 100) > 1e-7) return null;
  if (sizes[1] === 0) return { collapsed: true, width: null };
  const width = sizes[1] * layout.extent / 100;
  if (width < INSPECTOR_MIN_WIDTH - 1e-7 || width > layout.maxWidth + 1e-7) return null;
  return { collapsed: false, width };
}
