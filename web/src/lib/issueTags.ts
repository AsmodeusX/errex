// The issue list's view of an issue's tags: the latest event's tags, which
// the daemon puts on each issue (`Issue.tags`). The row shows the error code
// and context; the `tag:` token filters on any of them.

import type { TagFilter } from './queryParser';

/** Tags the issue row shows, in this order. */
const SUMMARY_KEYS = ['error.code', 'error.context'];

/** The error code and context of the issue's latest event, when present. */
export function issueSummaryTags(
  tags: Record<string, string> | null | undefined
): Array<[string, string]> {
  if (!tags) return [];
  return SUMMARY_KEYS.flatMap((k): Array<[string, string]> => {
    const v = tags[k];
    return v ? [[k, v]] : [];
  });
}

function valueMatches(value: string, wanted: string): boolean {
  const v = value.toLowerCase();
  const w = wanted.toLowerCase();
  if (!w.includes('*')) return v === w;
  const escaped = w.split('*').map((part) => part.replace(/[.+?^${}()|[\]\\]/g, '\\$&'));
  const re = new RegExp(`^${escaped.join('.*')}$`);
  return re.test(v);
}

/**
 * Whether `tags` satisfies every filter. A filter with a key needs that tag
 * (case-insensitive key) to equal one of its values; one without a key
 * needs any tag to. Values compare case-insensitively, `*` matching any run.
 */
export function matchesTagFilters(
  tags: Record<string, string> | null | undefined,
  filters: TagFilter[]
): boolean {
  if (filters.length === 0) return true;
  if (!tags) return false;
  const entries = Object.entries(tags).map(([k, v]): [string, string] => [k.toLowerCase(), v]);
  return filters.every((f) => {
    const key = f.key?.toLowerCase() ?? null;
    return entries.some(
      ([k, v]) => (key === null || k === key) && f.values.some((w) => valueMatches(v, w))
    );
  });
}
