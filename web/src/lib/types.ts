// Wire types mirror crates/errex-proto. Keep field names in lockstep with the
// Rust serde representation — issue.rs, event.rs, wire.rs.

export type IssueLevel = 'debug' | 'info' | 'warning' | 'error' | 'fatal';

/** Server-side triage state. Mirrors `errex_proto::IssueStatus`. */
export type IssueStatus = 'unresolved' | 'resolved' | 'muted' | 'ignored';

export interface Issue {
  id: number;
  project: string;
  fingerprint: string;
  title: string;
  culprit: string | null;
  level: string | null;
  status: IssueStatus;
  event_count: number;
  first_seen: string;
  last_seen: string;
  /**
   * Optional, server-supplied. `'txn'` means a Sentry transaction (perf
   * span) rather than an error event — the row template renders a `›`
   * glyph instead of a level dot. Absent today; reserved for a future
   * `errex-proto` bump so the UI is forward-compatible.
   */
  kind?: 'error' | 'txn';
  /** Distinct affected users in the active window. Optional. */
  user_count?: number;
  /** Set by the daemon when an issue re-fired after being resolved. */
  regressed?: boolean;
  /** Release of the first event that carried one. */
  first_release?: string | null;
  /** Release of the most recent event that carried one. */
  last_release?: string | null;
  /** Newest release, in the daemon's release order, that reported the issue. */
  newest_release?: string | null;
  /** While resolved: events from this release or older count without
   *  regressing the issue. */
  resolved_in_release?: string | null;
  /** Tags of the issue's most recent event, bounded by the daemon; absent
   *  when it has none. Drives the list's code/context line and `tag:`. */
  tags?: Record<string, string> | null;
}

/** One release that reported an issue (`GET /api/issues/:id/releases`). */
export interface IssueRelease {
  release: string;
  event_count: number;
  first_seen: string;
  last_seen: string;
}

/** One release a project has reported (`GET /api/projects/:name/releases`). */
export interface ProjectRelease {
  release: string;
  first_seen: string;
}

export interface ProjectSummary {
  project: string;
  issue_count: number;
}

// Server → client WebSocket frames. Tag matches `#[serde(tag = "type",
// rename_all = "snake_case")]` on errex_proto::ServerMessage.
export type ServerMessage =
  | { type: 'hello'; server_version: string }
  | { type: 'snapshot'; issues: Issue[] }
  | { type: 'issue_created'; issue: Issue }
  | { type: 'issue_updated'; issue: Issue };

export type ClientMessage = { type: 'ping' };

export type ConnectionStatus = 'connecting' | 'connected' | 'reconnecting' | 'disconnected';

/** Issue list ordering. Mirrored as a URL token in lib/filterUrl.ts. */
export type SortKey = 'recent' | 'stale' | 'count';

// ----- Event detail -----
//
// Mirrors errex_proto::Event for the fields the UI renders. The daemon
// keeps every field the SDK sent (after server-side scrubbing), so fields
// no section models (debug_meta, sdk, mechanism, instruction_addr, ...) are
// reachable through the index signatures and shown as additional data.

export interface Frame {
  filename?: string | null;
  function?: string | null;
  module?: string | null;
  lineno?: number | null;
  colno?: number | null;
  in_app?: boolean | null;
  /** Some SDKs emit `vars` per frame; surfaced verbatim. */
  vars?: Record<string, unknown>;
  /** Native and AOT frames: the address offline symbolication reads. */
  instruction_addr?: string | null;
  [key: string]: unknown;
}

export interface ExceptionInfo {
  type?: string | null;
  value?: string | null;
  module?: string | null;
  stacktrace?: { frames?: Frame[] } | null;
  /** How the error was captured (`handled`, `type`, ...). */
  mechanism?: Record<string, unknown> | null;
  [key: string]: unknown;
}

export interface Breadcrumb {
  /**
   * Sentry's wire format allows two shapes here: an ISO-8601 string (the
   * spec'd canonical form) and a numeric Unix epoch in seconds (what most
   * JS SDKs actually emit, sometimes with sub-second precision). errex
   * preserves whichever the SDK sent — render-side helpers normalize.
   */
  timestamp?: string | number | null;
  category?: string | null;
  level?: IssueLevel | null;
  message?: string | null;
  type?: string | null;
  /** Structured fields of the crumb (`page`, `action`, `endpoint`, ...). */
  data?: Record<string, unknown> | null;
}

/** Sentry's envelope-event payload, as preserved by errexd. */
export interface EventPayload {
  event_id?: string;
  timestamp?: string;
  platform?: string | null;
  level?: IssueLevel | null;
  environment?: string | null;
  release?: string | null;
  server_name?: string | null;
  message?: string | null;
  exception?: { values?: ExceptionInfo[] } | null;
  breadcrumbs?: { values?: Breadcrumb[] } | Breadcrumb[] | null;
  tags?: Record<string, string> | Array<[string, string]> | null;
  extra?: Record<string, unknown> | null;
  contexts?: Record<string, unknown> | null;
  [key: string]: unknown;
}

/** What `/api/issues/:id/event` returns. */
export interface StoredEvent {
  event_id: string;
  received_at: string;
  payload: EventPayload;
}

// ----- View-model shapes the components render -----
//
// Components stay decoupled from the wire format: the `selection.event`
// store is normalized into these shapes by `lib/eventDetail.ts` so the
// templates don't have to dig through optional Sentry-style nesting.

export interface Stack {
  type: string | null;
  value: string | null;
  frames: Frame[];
}

export interface NormalizedEvent {
  event_id: string;
  received_at: string;
  level: IssueLevel | null;
  release: string | null;
  environment: string | null;
  exception: Stack | null;
  breadcrumbs: Breadcrumb[];
  tags: Record<string, string>;
  /** Untouched payload as returned by the daemon. Kept so consumers can
   *  reach fields the normalized view-model intentionally drops (contexts,
   *  user, request, message) — used by the AI-context exporter. */
  raw: StoredEvent;
}
