// Adapters from the raw Sentry-style envelope payload into the view-model
// shapes the components render. Sentry has accreted multiple "shapes" for
// breadcrumbs and tags over the years, so we normalize them here once
// rather than scattering the conditionals across templates.

import type {
  Breadcrumb,
  EventPayload,
  ExceptionInfo,
  Frame,
  IssueLevel,
  NormalizedEvent,
  Stack,
  StoredEvent
} from './types';

function isRecord(v: unknown): v is Record<string, unknown> {
  return typeof v === 'object' && v !== null && !Array.isArray(v);
}

export function normalize(stored: StoredEvent): NormalizedEvent {
  const p = stored.payload ?? {};
  return {
    event_id: stored.event_id,
    received_at: stored.received_at,
    level: (p.level as IssueLevel | null | undefined) ?? null,
    release: p.release ?? null,
    environment: p.environment ?? null,
    exception: extractStack(p),
    breadcrumbs: extractBreadcrumbs(p),
    tags: extractTags(p),
    raw: stored
  };
}

function extractStack(p: EventPayload): Stack | null {
  const ex = p.exception?.values?.[0];
  if (!ex) return null;
  return {
    type: ex.type ?? null,
    value: ex.value ?? null,
    frames: ex.stacktrace?.frames ?? []
  };
}

function extractBreadcrumbs(p: EventPayload): Breadcrumb[] {
  const bc = p.breadcrumbs;
  if (!bc) return [];
  // Two shapes seen in the wild: `{ values: [...] }` (newer SDKs) and a
  // bare array (older Python SDK, some custom integrations).
  if (Array.isArray(bc)) return bc;
  return bc.values ?? [];
}

function extractTags(p: EventPayload): Record<string, string> {
  const t = p.tags;
  if (!t) return {};
  if (Array.isArray(t)) {
    const out: Record<string, string> = {};
    for (const pair of t) {
      if (Array.isArray(pair) && pair.length >= 2) out[String(pair[0])] = String(pair[1]);
    }
    return out;
  }
  // Object form: stringify values defensively — some SDKs emit numbers.
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(t)) out[k] = String(v);
  return out;
}

/**
 * Drop redundant `x.name` keys when `x` is already present. Sentry SDKs
 * commonly emit both — `os` carrying "macOS 15.3" and `os.name` carrying
 * "macOS" — which doubles the badge wall for no information gain. Same
 * goes for `device.family` when `device` already names the model.
 */
export function dedupTags(tags: Record<string, string>): Record<string, string> {
  const keys = new Set(Object.keys(tags));
  const out: Record<string, string> = {};
  for (const [k, v] of Object.entries(tags)) {
    const dot = k.indexOf('.');
    if (dot > 0) {
      const prefix = k.slice(0, dot);
      const suffix = k.slice(dot + 1);
      if ((suffix === 'name' || suffix === 'family') && keys.has(prefix)) continue;
    }
    out[k] = v;
  }
  return out;
}

/**
 * Coerce a Sentry timestamp to milliseconds-since-epoch.
 *
 * Sentry SDKs are inconsistent: the spec asks for ISO-8601 strings, but
 * most JS SDKs ship Unix epoch *seconds* (often as a float with sub-second
 * precision). Older Python integrations emit ISO strings; some custom
 * forwarders emit milliseconds. We accept all three forms — falling back
 * to NaN for anything else so callers can render the em-dash placeholder.
 */
export function parseSentryTimestamp(ts: string | number | null | undefined): number {
  if (ts === null || ts === undefined) return Number.NaN;
  if (typeof ts === 'number') {
    if (!Number.isFinite(ts)) return Number.NaN;
    // Heuristic: anything below ~10^12 is seconds-since-epoch (year 33658
    // in milliseconds is the cutoff; year 2001 in seconds is 10^9). Above
    // that, treat as already-milliseconds.
    return ts < 1e12 ? ts * 1000 : ts;
  }
  // Numeric strings ("1729012345.678") slip in from a few SDKs that
  // stringify the Unix-seconds float. Date.parse returns NaN on those, so
  // try parseFloat first when the input has no separator that ISO needs.
  if (!ts.includes('-') && !ts.includes('T') && !ts.includes(':')) {
    const n = Number.parseFloat(ts);
    if (Number.isFinite(n)) return n < 1e12 ? n * 1000 : n;
  }
  const ms = Date.parse(ts);
  return Number.isFinite(ms) ? ms : Number.NaN;
}

/**
 * Format a breadcrumb timestamp as a delta against the crash event so the
 * user reads "-42s" instead of doing wall-clock math against an absolute
 * `01:18:49.203`. T-0 is reserved for the breadcrumb that lands on the
 * crash itself; positive deltas (rare — usually clock skew) get a `+`.
 */
export function breadcrumbRelativeTime(
  crashTs: string | number | null | undefined,
  bcTs: string | number | null | undefined
): string {
  const crash = parseSentryTimestamp(crashTs);
  const bc = parseSentryTimestamp(bcTs);
  if (!Number.isFinite(crash) || !Number.isFinite(bc)) return '—';
  const deltaMs = bc - crash;
  if (deltaMs === 0) return 'T-0';
  const sign = deltaMs > 0 ? '+' : '-';
  const absSec = Math.round(Math.abs(deltaMs) / 1000);
  if (absSec < 90) return `${sign}${absSec}s`;
  const m = Math.floor(absSec / 60);
  const s = absSec % 60;
  return s === 0 ? `${sign}${m}m` : `${sign}${m}m${s}s`;
}

/**
 * Split frames into "your code" (in_app=true) vs "dependencies"
 * (in_app=false / null / undefined). The header `N in your code` is the
 * fastest way to answer "is this mine to fix?" before reading the trace.
 */
export function partitionFrames(frames: Frame[]): { inApp: number; lib: number } {
  let inApp = 0;
  let lib = 0;
  for (const f of frames) {
    if (f.in_app === true) inApp += 1;
    else lib += 1;
  }
  return { inApp, lib };
}

/**
 * Index of the "throw site" — the frame the eye should land on first.
 * Sentry orders frames oldest-first, so the innermost call (= where the
 * exception was raised) is the last in_app frame. If none are flagged
 * in_app we fall back to the last frame so we still pre-expand something
 * useful instead of leaving the trace fully collapsed.
 */
export function throwSiteIndex(frames: Frame[]): number {
  if (frames.length === 0) return -1;
  for (let i = frames.length - 1; i >= 0; i -= 1) {
    if (frames[i]?.in_app === true) return i;
  }
  return frames.length - 1;
}

// Top-level payload keys a dedicated section (header, stack trace,
// breadcrumbs, tags) already renders, or that never carry content.
const RENDERED_KEYS = new Set([
  'event_id',
  'timestamp',
  'platform',
  'level',
  'environment',
  'release',
  'server_name',
  'message',
  'logentry',
  'fingerprint',
  'exception',
  'breadcrumbs',
  'tags',
  'extra',
  'contexts'
]);

// Frame keys the frame row itself shows; `vars` has its own table.
const FRAME_ROW_KEYS = new Set([
  'filename',
  'function',
  'module',
  'lineno',
  'colno',
  'in_app',
  'vars'
]);

function isEmpty(v: unknown): boolean {
  if (v === null || v === undefined || v === '') return true;
  if (Array.isArray(v)) return v.length === 0;
  if (typeof v === 'object') return Object.keys(v as object).length === 0;
  return false;
}

// `extra` keys the stack and cause sections render.
const RENDERED_EXTRA = new Set(['dart_stack', 'build_id', 'causes']);

/**
 * Payload fields no dedicated section renders: each remaining `extra` key
 * (the stack, build id and causes have their own section), the primary
 * exception's `mechanism`, then every field the SDK sent that errex does
 * not model (`debug_meta`, `sdk`, `dist`, `threads`, ...), alphabetically.
 * Empty values are skipped.
 */
export function additionalData(p: EventPayload): Array<{ key: string; value: unknown }> {
  const out: Array<{ key: string; value: unknown }> = [];
  const push = (key: string, value: unknown) => {
    if (!isEmpty(value)) out.push({ key, value });
  };
  if (isRecord(p.extra)) {
    for (const k of Object.keys(p.extra).sort()) {
      if (!RENDERED_EXTRA.has(k)) push(`extra.${k}`, p.extra[k]);
    }
  } else {
    push('extra', p.extra);
  }
  const ex: ExceptionInfo | undefined = p.exception?.values?.[0];
  push('exception.mechanism', ex?.mechanism);
  const unknown = Object.keys(p)
    .filter((k) => !RENDERED_KEYS.has(k))
    .sort();
  for (const k of unknown) push(k, p[k]);
  return out;
}

/** A frame's fields the frame row does not show, sorted by key. */
export function frameDetails(f: Frame): Array<[string, string]> {
  return Object.keys(f)
    .filter((k) => !FRAME_ROW_KEYS.has(k) && !isEmpty(f[k]))
    .sort()
    .map((k) => {
      const v = f[k];
      return [k, typeof v === 'string' ? v : JSON.stringify(v)];
    });
}

/** Strings verbatim (a trace keeps its line breaks), structures pretty-printed. */
export function formatData(v: unknown): string {
  return typeof v === 'string' ? v : JSON.stringify(v, null, 2);
}

// ----- Tags -----

export interface TagGroup {
  id: string;
  label: string;
  entries: Array<[string, string]>;
}

// The app's session tags: where and how it ran, not what failed.
const SESSION_TAGS = new Set([
  'recovery.pending',
  'network.state',
  'app.distribution',
  'device.class',
  'os',
  'os.name',
  'os.version'
]);

const NAMED_GROUPS: Array<{ id: string; label: string }> = [
  { id: 'error', label: 'Error' },
  { id: 'api', label: 'API' },
  { id: 'ecu', label: 'ECU' },
  { id: 'session', label: 'Session' }
];

// Keys read first within their group; the rest follow alphabetically.
const LEAD_KEYS = ['error.code', 'error.context', 'error.detail', 'api.endpoint', 'api.status', 'api.code', 'api.kind'];

function tagGroupId(key: string): string {
  if (SESSION_TAGS.has(key)) return 'session';
  const dot = key.indexOf('.');
  return dot > 0 ? key.slice(0, dot) : 'other';
}

function compareTagKeys(a: string, b: string): number {
  const ia = LEAD_KEYS.indexOf(a);
  const ib = LEAD_KEYS.indexOf(b);
  if (ia !== -1 || ib !== -1) return (ia === -1 ? Infinity : ia) - (ib === -1 ? Infinity : ib);
  return a < b ? -1 : a > b ? 1 : 0;
}

/**
 * Tags grouped for the event view: error, API, ECU and session first, then
 * one group per remaining key prefix (`flash`, `transport`, ...) in
 * alphabetical order, and keys without a prefix last as `other`.
 */
export function groupTags(tags: Record<string, string>): TagGroup[] {
  const byId = new Map<string, Array<[string, string]>>();
  for (const [k, v] of Object.entries(tags)) {
    const id = tagGroupId(k);
    const list = byId.get(id) ?? [];
    list.push([k, v]);
    byId.set(id, list);
  }
  const named = new Set(NAMED_GROUPS.map((g) => g.id));
  const rest = [...byId.keys()].filter((id) => !named.has(id) && id !== 'other').sort();
  const order: Array<{ id: string; label: string }> = [
    ...NAMED_GROUPS,
    ...rest.map((id) => ({ id, label: id })),
    { id: 'other', label: 'Other' }
  ];
  const out: TagGroup[] = [];
  for (const g of order) {
    const entries = byId.get(g.id);
    if (!entries) continue;
    entries.sort(([a], [b]) => compareTagKeys(a, b));
    out.push({ ...g, entries });
  }
  return out;
}

// ----- Dart stacks and causes -----

const BUILD_ID_LINE = /^build_id: '?([0-9a-fA-F]{8,64})'?/m;
const BUILD_ID = /^[0-9a-fA-F]{8,64}$/;

/** The build id a non-symbolic Dart stack's header names, lower-cased. */
export function stackBuildId(stack: string): string | null {
  return BUILD_ID_LINE.exec(stack)?.[1]?.toLowerCase() ?? null;
}

/**
 * The event's primary obfuscated Dart stack (`extra.dart_stack`) and the
 * build id that picks its symbols: `extra.build_id` when it is one, else the
 * id the stack's header names.
 */
export function primaryDartStack(p: EventPayload): { stack: string | null; buildId: string | null } {
  const extra = isRecord(p.extra) ? p.extra : {};
  const stack = typeof extra.dart_stack === 'string' && extra.dart_stack !== '' ? extra.dart_stack : null;
  if (stack === null) return { stack: null, buildId: null };
  const sent = extra.build_id;
  const buildId = typeof sent === 'string' && BUILD_ID.test(sent) ? sent : stackBuildId(stack);
  return { stack, buildId };
}

export interface Cause {
  type: string | null;
  message: string | null;
  stack: string | null;
}

/** Most causes rendered; the app sends at most five. */
const CAUSES_MAX = 16;

/** `extra.causes`, outer to inner, as sent: `{type, message, dart_stack}`. */
export function causeChain(p: EventPayload): Cause[] {
  const causes = isRecord(p.extra) ? p.extra.causes : undefined;
  if (!Array.isArray(causes)) return [];
  const str = (v: unknown) => (typeof v === 'string' && v !== '' ? v : null);
  return causes
    .filter(isRecord)
    .slice(0, CAUSES_MAX)
    .map((c) => ({ type: str(c.type), message: str(c.message), stack: str(c.dart_stack) }));
}

// ----- Breadcrumbs -----

/** A breadcrumb's `data` fields in the order sent, values as text. */
export function breadcrumbData(bc: Breadcrumb): Array<[string, string]> {
  if (!isRecord(bc.data)) return [];
  return Object.entries(bc.data).map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)]);
}

/**
 * Breadcrumbs oldest first. Ties keep the order they were sent in, and a
 * crumb without a usable timestamp stays right after the one sent before it.
 */
export function breadcrumbsInTimeOrder(bcs: Breadcrumb[]): Breadcrumb[] {
  let last = Number.NEGATIVE_INFINITY;
  const keyed = bcs.map((bc, i) => {
    const t = parseSentryTimestamp(bc.timestamp);
    if (Number.isFinite(t)) last = t;
    return { bc, i, t: Number.isFinite(t) ? t : last };
  });
  keyed.sort((a, b) => a.t - b.t || a.i - b.i);
  return keyed.map((k) => k.bc);
}

// ----- Contexts -----

/** Each context (`os`, `device`, ...) by name, with its fields as text. */
export function contextSections(
  contexts: Record<string, unknown> | null | undefined
): Array<{ name: string; fields: Array<[string, string]> }> {
  if (!isRecord(contexts)) return [];
  return Object.keys(contexts)
    .sort()
    .flatMap((name) => {
      const c = contexts[name];
      if (isEmpty(c)) return [];
      const fields: Array<[string, string]> = isRecord(c)
        ? Object.entries(c)
            .filter(([, v]) => !isEmpty(v))
            .map(([k, v]) => [k, typeof v === 'string' ? v : JSON.stringify(v)])
        : [['value', typeof c === 'string' ? c : JSON.stringify(c)]];
      return fields.length > 0 ? [{ name, fields }] : [];
    });
}

// ----- Raw event -----

/** The event as `tool/symbolize.dart` reads it: the payload, pretty-printed. */
export function symbolicationJson(stored: StoredEvent): string {
  return JSON.stringify(stored.payload, null, 2);
}

/** File name for a downloaded event, from its id. */
export function eventFileName(eventId: string): string {
  return `errex-event-${eventId.replace(/[^A-Za-z0-9-]/g, '')}.json`;
}
