// Serialize/parse the issue list filter to/from URLSearchParams so the
// active filter is shareable, reload-safe, and bookmarkable. Defaults
// are omitted from the query string to keep clean URLs the common case.

import type { TagFilter } from './queryParser';
import type { IssueLevel, IssueStatus, SortKey } from './types';

export interface FilterState {
  query: string;
  statuses: Set<IssueStatus>;
  levels: Set<IssueLevel>;
  sinceMs: number | null;
  spikingOnly: boolean;
  sort: SortKey;
  /** Maximum number of issues to surface; null = no cap. Driven by `top N`
   *  / `limit:N` tokens in the unified input. */
  limit?: number | null;
  /** `tag:` filters, one `tag` param each (`key:v1,v2`, or `v` without a key). */
  tags?: TagFilter[];
}

// The characters a `tag:` token accepts (queryParser), so a URL cannot
// carry a filter the input could not show.
const TAG_KEY = /^[\w.-]+$/;
const TAG_VALUE = /^[\w.*-]+$/;

function parseTagParam(raw: string): TagFilter | null {
  const colon = raw.indexOf(':');
  const key = colon === -1 ? null : raw.slice(0, colon).toLowerCase();
  const values = (colon === -1 ? raw : raw.slice(colon + 1))
    .split(',')
    .map((v) => v.trim().toLowerCase())
    .filter(Boolean);
  if (values.length === 0 || !values.every((v) => TAG_VALUE.test(v))) return null;
  if (key !== null && !TAG_KEY.test(key)) return null;
  return { key, values };
}

const ALL_STATUSES: IssueStatus[] = ['unresolved', 'resolved', 'muted', 'ignored'];
const ALL_LEVELS: IssueLevel[] = ['debug', 'info', 'warning', 'error', 'fatal'];
export const ALL_SORTS: SortKey[] = ['recent', 'stale', 'count'];

// Whitelisted "since" presets. Keeping the URL token symbolic (1h/24h/7d)
// rather than raw milliseconds means a stale share link can't smuggle in
// an arbitrary window, and the encoded URL stays human-readable.
const SINCE_PRESETS: Record<string, number> = {
  '5m': 5 * 60 * 1000,
  '15m': 15 * 60 * 1000,
  '1h': 60 * 60 * 1000,
  '24h': 24 * 60 * 60 * 1000,
  '7d': 7 * 24 * 60 * 60 * 1000
};

function setEqualsDefaultStatuses(s: Set<IssueStatus>): boolean {
  return s.size === 1 && s.has('unresolved');
}

function csvSorted<T extends string>(s: Set<T>): string {
  return [...s].sort().join(',');
}

function sinceMsToToken(ms: number): string | null {
  for (const [token, value] of Object.entries(SINCE_PRESETS)) {
    if (value === ms) return token;
  }
  return null;
}

export function serializeFilterParams(f: FilterState): URLSearchParams {
  const p = new URLSearchParams();
  if (f.query.trim().length > 0) p.set('q', f.query);
  if (!setEqualsDefaultStatuses(f.statuses)) p.set('s', csvSorted(f.statuses));
  if (f.levels.size > 0) p.set('l', csvSorted(f.levels));
  if (f.sinceMs != null) {
    const token = sinceMsToToken(f.sinceMs);
    if (token) p.set('since', token);
  }
  if (f.spikingOnly) p.set('spike', '1');
  if (f.sort !== 'recent') p.set('sort', f.sort);
  // Limit is small-integer-only (1..10000). We cap on parse so a stale
  // share link can't pin a million-row scan onto the client.
  if (f.limit != null && f.limit > 0) p.set('limit', String(f.limit));
  for (const t of f.tags ?? []) {
    p.append('tag', t.key === null ? t.values.join(',') : `${t.key}:${t.values.join(',')}`);
  }
  return p;
}

export function parseFilterParams(p: URLSearchParams): FilterState {
  const query = p.get('q') ?? '';

  const sRaw = p.get('s');
  let statuses: Set<IssueStatus>;
  if (sRaw == null) {
    statuses = new Set<IssueStatus>(['unresolved']);
  } else {
    statuses = new Set<IssueStatus>(
      sRaw
        .split(',')
        .map((t) => t.trim())
        .filter((t): t is IssueStatus => (ALL_STATUSES as string[]).includes(t))
    );
    // Empty after filtering invalid tokens — fall back rather than show
    // nothing on a malformed share link.
    if (statuses.size === 0) statuses = new Set<IssueStatus>(['unresolved']);
  }

  const lRaw = p.get('l');
  const levels =
    lRaw == null
      ? new Set<IssueLevel>()
      : new Set<IssueLevel>(
          lRaw
            .split(',')
            .map((t) => t.trim())
            .filter((t): t is IssueLevel => (ALL_LEVELS as string[]).includes(t))
        );

  const sinceRaw = p.get('since');
  const sinceMs = sinceRaw != null && sinceRaw in SINCE_PRESETS ? SINCE_PRESETS[sinceRaw]! : null;

  const spikingOnly = p.get('spike') === '1';

  const sortRaw = p.get('sort');
  const sort: SortKey =
    sortRaw != null && (ALL_SORTS as string[]).includes(sortRaw)
      ? (sortRaw as SortKey)
      : 'recent';

  const limitRaw = p.get('limit');
  let limit: number | null = null;
  if (limitRaw != null) {
    const n = parseInt(limitRaw, 10);
    if (Number.isFinite(n) && n > 0 && n <= 10000) limit = n;
  }

  const tags = p
    .getAll('tag')
    .map(parseTagParam)
    .filter((t): t is TagFilter => t !== null);

  return { query, statuses, levels, sinceMs, spikingOnly, sort, limit, tags };
}
