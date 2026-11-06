import { describe, expect, it } from 'vitest';
import {
  additionalData,
  breadcrumbData,
  breadcrumbRelativeTime,
  breadcrumbsInTimeOrder,
  causeChain,
  contextSections,
  dedupTags,
  eventFileName,
  formatData,
  frameDetails,
  groupTags,
  partitionFrames,
  primaryDartStack,
  stackBuildId,
  symbolicationJson,
  throwSiteIndex
} from './eventDetail';
import type { Breadcrumb, EventPayload, Frame, StoredEvent } from './types';

describe('dedupTags', () => {
  it('drops `x.name` when `x` is also present', () => {
    const out = dedupTags({
      browser: 'Chrome 134',
      'browser.name': 'Chrome',
      os: 'macOS 15.3',
      'os.name': 'macOS'
    });
    expect(out).toEqual({ browser: 'Chrome 134', os: 'macOS 15.3' });
  });

  it('keeps unrelated `.name` keys', () => {
    const out = dedupTags({ 'feature.name': 'billing' });
    expect(out).toEqual({ 'feature.name': 'billing' });
  });

  it('drops `device.family` when `device` is present', () => {
    const out = dedupTags({ device: 'MacBookPro18,3', 'device.family': 'Mac' });
    expect(out).toEqual({ device: 'MacBookPro18,3' });
  });

  it('handles empty input', () => {
    expect(dedupTags({})).toEqual({});
  });
});

describe('breadcrumbRelativeTime', () => {
  const crash = '2026-04-27T04:19:31.000Z';

  it('returns T-0 for the crash timestamp itself', () => {
    expect(breadcrumbRelativeTime(crash, crash)).toBe('T-0');
  });

  it('returns negative seconds for breadcrumbs before the crash', () => {
    expect(breadcrumbRelativeTime(crash, '2026-04-27T04:18:49.000Z')).toBe('-42s');
    expect(breadcrumbRelativeTime(crash, '2026-04-27T04:19:30.000Z')).toBe('-1s');
  });

  it('falls back to em-dash when breadcrumb timestamp is missing', () => {
    expect(breadcrumbRelativeTime(crash, undefined)).toBe('—');
    expect(breadcrumbRelativeTime(crash, null)).toBe('—');
  });

  it('switches to minutes past 90 seconds', () => {
    expect(breadcrumbRelativeTime(crash, '2026-04-27T04:17:00.000Z')).toBe('-2m31s');
    expect(breadcrumbRelativeTime(crash, '2026-04-27T04:00:00.000Z')).toBe('-19m31s');
  });

  it('handles breadcrumbs after the crash with positive prefix', () => {
    expect(breadcrumbRelativeTime(crash, '2026-04-27T04:19:33.000Z')).toBe('+2s');
  });

  it('accepts Unix-seconds floats from JS SDKs (the in-the-wild format)', () => {
    // Sentry JS SDK ships breadcrumb.timestamp as a numeric float in
    // seconds, NOT an ISO string — every screenshot of "—" in the
    // breadcrumb table came from Date.parse silently returning NaN on
    // these inputs.
    const crashEpoch = Date.parse(crash) / 1000;
    expect(breadcrumbRelativeTime(crashEpoch, crashEpoch - 42)).toBe('-42s');
    expect(breadcrumbRelativeTime(crashEpoch, crashEpoch + 2)).toBe('+2s');
    expect(breadcrumbRelativeTime(crashEpoch, crashEpoch)).toBe('T-0');
  });

  it('mixes formats: ISO crash + numeric breadcrumb (or vice versa)', () => {
    const crashEpoch = Date.parse(crash) / 1000;
    expect(breadcrumbRelativeTime(crash, crashEpoch - 42)).toBe('-42s');
    expect(breadcrumbRelativeTime(crashEpoch, '2026-04-27T04:18:49.000Z')).toBe('-42s');
  });

  it('accepts numeric strings (some custom forwarders stringify the float)', () => {
    const crashEpoch = String(Date.parse(crash) / 1000);
    const bcEpoch = String(Date.parse(crash) / 1000 - 42);
    expect(breadcrumbRelativeTime(crashEpoch, bcEpoch)).toBe('-42s');
    expect(breadcrumbRelativeTime(`${crashEpoch}.5`, `${bcEpoch}.5`)).toBe('-42s');
  });

  it('accepts millisecond-scale numbers (Date.now() shape)', () => {
    const crashMs = Date.parse(crash);
    expect(breadcrumbRelativeTime(crashMs, crashMs - 42_000)).toBe('-42s');
  });
});

describe('partitionFrames', () => {
  function frame(in_app: boolean | null | undefined): Frame {
    return { function: 'f', filename: 'a.ts', in_app };
  }

  it('counts in_app vs library frames', () => {
    const out = partitionFrames([frame(true), frame(true), frame(false), frame(null)]);
    expect(out).toEqual({ inApp: 2, lib: 2 });
  });

  it('treats undefined/null in_app as library frames', () => {
    expect(partitionFrames([frame(undefined)])).toEqual({ inApp: 0, lib: 1 });
  });

  it('handles empty input', () => {
    expect(partitionFrames([])).toEqual({ inApp: 0, lib: 0 });
  });
});

describe('throwSiteIndex', () => {
  function frame(in_app: boolean | null | undefined, name = 'f'): Frame {
    return { function: name, filename: 'a.ts', in_app };
  }

  it('returns the last in_app frame index', () => {
    // Sentry orders frames oldest-first, so the throw site is the
    // last in-app entry — that's the row the user wants pre-expanded.
    const frames = [frame(false, 'lib1'), frame(true, 'app1'), frame(true, 'app2'), frame(false, 'lib2')];
    expect(throwSiteIndex(frames)).toBe(2);
  });

  it('falls back to the last frame when no in_app frames exist', () => {
    expect(throwSiteIndex([frame(false), frame(false)])).toBe(1);
  });

  it('returns -1 for empty input', () => {
    expect(throwSiteIndex([])).toBe(-1);
  });

  it('treats null/undefined in_app as library frames for ranking', () => {
    expect(throwSiteIndex([frame(null), frame(undefined), frame(true)])).toBe(2);
  });
});

describe('additionalData', () => {
  it('lists fields the dedicated sections do not render', () => {
    const p: EventPayload = {
      message: 'm',
      release: 'r',
      tags: { a: 'b' },
      extra: {
        dart_stack: '#00 abs 0000007a',
        build_id: '0f3a5b7c',
        causes: [{ type: 'flash_failure' }],
        transport_trace: [{ dir: 'tx' }],
        app_build: 29
      },
      contexts: { os: { name: 'Android' } },
      sdk: { name: 'zeus.reporter' },
      debug_meta: { images: [] },
      dist: '45',
      exception: {
        values: [{ type: 'E', mechanism: { type: 'FlutterError', handled: false } }]
      }
    };
    const keys = additionalData(p).map((e) => e.key);
    // The stack, build id, causes and contexts have their own sections.
    expect(keys).toEqual([
      'extra.app_build',
      'extra.transport_trace',
      'exception.mechanism',
      'debug_meta',
      'dist',
      'sdk'
    ]);
  });

  it('keeps an extra that is not an object whole', () => {
    const p = { extra: 'note' } as unknown as EventPayload;
    expect(additionalData(p)).toEqual([{ key: 'extra', value: 'note' }]);
  });

  it('skips empty and null values', () => {
    const p = { extra: {}, contexts: null, sdk: undefined, threads: [] } as EventPayload;
    expect(additionalData(p)).toEqual([]);
  });

  it('returns nothing for a payload with only modelled fields', () => {
    expect(additionalData({ message: 'm', level: 'error' })).toEqual([]);
  });
});

describe('frameDetails', () => {
  it('returns the fields a frame row does not show', () => {
    const f: Frame = {
      function: 'f',
      filename: 'a.dart',
      lineno: 3,
      in_app: true,
      instruction_addr: '0x2f5c1f',
      addr_mode: 'rel:0',
      vars: { x: 1 }
    };
    expect(frameDetails(f)).toEqual([
      ['addr_mode', 'rel:0'],
      ['instruction_addr', '0x2f5c1f']
    ]);
  });

  it('stringifies non-string values', () => {
    expect(frameDetails({ function: 'f', symbol_addr: 16, flags: { a: 1 } })).toEqual([
      ['flags', '{"a":1}'],
      ['symbol_addr', '16']
    ]);
  });
});

describe('formatData', () => {
  it('keeps strings verbatim so traces keep their line breaks', () => {
    expect(formatData('#00 abs\n#01 abs')).toBe('#00 abs\n#01 abs');
  });

  it('pretty-prints structures', () => {
    expect(formatData({ a: 1 })).toBe('{\n  "a": 1\n}');
  });
});

const STACK =
  "build_id: '0f3a5b7c9d1e2f3a4152c37485e60718'\n" +
  '#00 abs 000000750a2f5c1f virt 00000000002f5c1f _kDartIsolateSnapshotInstructions+0x2f5c1f';

describe('groupTags', () => {
  it('groups error, api, ecu and session tags first, then the rest by prefix', () => {
    const groups = groupTags({
      'os.version': '14',
      'error.detail': 'm1a2b3c4d',
      'api.code': 'bad_credentials',
      'ecu.board': 'msd81',
      'flash.stage': 'erase',
      'error.code': 'state_error',
      'device.class': 'phone',
      'error.context': 'flash_plan',
      'recovery.pending': 'false',
      'network.state': 'online',
      'app.distribution': 'play',
      'api.kind': 'defect',
      'ecu.software.kind': 'ours',
      'ecu.release': '1.0.11',
      'recovery.status': 'none',
      os: 'android',
      plain: 'x'
    });
    expect(groups.map((g) => g.id)).toEqual(['error', 'api', 'ecu', 'session', 'flash', 'recovery', 'other']);
    expect(groups[0]!.entries).toEqual([
      ['error.code', 'state_error'],
      ['error.context', 'flash_plan'],
      ['error.detail', 'm1a2b3c4d']
    ]);
    expect(groups[2]!.entries.map(([k]) => k)).toEqual(['ecu.board', 'ecu.release', 'ecu.software.kind']);
    expect(groups[3]!.entries.map(([k]) => k)).toEqual([
      'app.distribution',
      'device.class',
      'network.state',
      'os',
      'os.version',
      'recovery.pending'
    ]);
    expect(groups[5]!.entries).toEqual([['recovery.status', 'none']]);
    expect(groups[6]!.entries).toEqual([['plain', 'x']]);
  });

  it('returns no groups for no tags', () => {
    expect(groupTags({})).toEqual([]);
  });
});

describe('primaryDartStack', () => {
  it('reads the stack and the build id from extra', () => {
    const p: EventPayload = { extra: { dart_stack: STACK, build_id: 'abcdef01' } };
    expect(primaryDartStack(p)).toEqual({ stack: STACK, buildId: 'abcdef01' });
  });

  it('falls back to the build id the stack header names', () => {
    expect(primaryDartStack({ extra: { dart_stack: STACK } })).toEqual({
      stack: STACK,
      buildId: '0f3a5b7c9d1e2f3a4152c37485e60718'
    });
  });

  it('ignores a scrubbed build id in favour of the header', () => {
    expect(primaryDartStack({ extra: { dart_stack: STACK, build_id: '<hex>' } }).buildId).toBe(
      '0f3a5b7c9d1e2f3a4152c37485e60718'
    );
  });

  it('is empty without a string stack', () => {
    expect(primaryDartStack({})).toEqual({ stack: null, buildId: null });
    expect(primaryDartStack({ extra: { dart_stack: 42 } })).toEqual({ stack: null, buildId: null });
    expect(primaryDartStack({ extra: { dart_stack: '' } })).toEqual({ stack: null, buildId: null });
  });
});

describe('stackBuildId', () => {
  it('reads quoted and bare build ids, lower-cased', () => {
    expect(stackBuildId("build_id: 'ABCDEF0123'")).toBe('abcdef0123');
    expect(stackBuildId('x\nbuild_id: abcdef0123\n#00')).toBe('abcdef0123');
    expect(stackBuildId('#00 abs 1')).toBeNull();
  });
});

describe('causeChain', () => {
  it('keeps the order and the fields the app sends', () => {
    const p: EventPayload = {
      extra: {
        causes: [
          { type: 'flash_failure', message: 'flash_failure', dart_stack: STACK },
          { type: 'host_refusal', message: 'host_refusal', dart_stack: '' }
        ]
      }
    };
    expect(causeChain(p)).toEqual([
      { type: 'flash_failure', message: 'flash_failure', stack: STACK },
      { type: 'host_refusal', message: 'host_refusal', stack: null }
    ]);
  });

  it('drops entries that are not objects and coerces nothing else', () => {
    const p = {
      extra: { causes: ['x', null, { type: 3, message: { a: 1 }, dart_stack: STACK }] }
    } as unknown as EventPayload;
    expect(causeChain(p)).toEqual([{ type: null, message: null, stack: STACK }]);
  });

  it('is empty when there are no causes', () => {
    expect(causeChain({})).toEqual([]);
    expect(causeChain({ extra: { causes: 'x' } })).toEqual([]);
  });

  it('is bounded', () => {
    const many = Array.from({ length: 100 }, () => ({ type: 't' }));
    expect(causeChain({ extra: { causes: many } })).toHaveLength(16);
  });
});

describe('breadcrumbData', () => {
  it('lists every data field, values as text', () => {
    const bc: Breadcrumb = {
      category: 'licensing',
      message: 'Licensing activate: 409',
      data: { endpoint: 'activate', status: '409', code: 'bad_credentials', kind: 'input', n: 3, o: { a: 1 } }
    };
    expect(breadcrumbData(bc)).toEqual([
      ['endpoint', 'activate'],
      ['status', '409'],
      ['code', 'bad_credentials'],
      ['kind', 'input'],
      ['n', '3'],
      ['o', '{"a":1}']
    ]);
  });

  it('is empty without data', () => {
    expect(breadcrumbData({ message: 'm' })).toEqual([]);
    expect(breadcrumbData({ data: null })).toEqual([]);
    expect(breadcrumbData({ data: 'x' } as unknown as Breadcrumb)).toEqual([]);
  });
});

describe('breadcrumbsInTimeOrder', () => {
  it('orders by timestamp, oldest first, keeping ties in sent order', () => {
    const bcs: Breadcrumb[] = [
      { message: 'c', timestamp: 30 },
      { message: 'a', timestamp: 10 },
      { message: 'b1', timestamp: '1970-01-01T00:00:20Z' },
      { message: 'b2', timestamp: 20 }
    ];
    expect(breadcrumbsInTimeOrder(bcs).map((b) => b.message)).toEqual(['a', 'b1', 'b2', 'c']);
  });

  it('keeps an undated breadcrumb after the one sent before it', () => {
    const bcs: Breadcrumb[] = [
      { message: 'b', timestamp: 20 },
      { message: 'b-undated' },
      { message: 'a', timestamp: 10 }
    ];
    expect(breadcrumbsInTimeOrder(bcs).map((b) => b.message)).toEqual(['a', 'b', 'b-undated']);
  });

  it('does not mutate its input', () => {
    const bcs: Breadcrumb[] = [{ timestamp: 2 }, { timestamp: 1 }];
    breadcrumbsInTimeOrder(bcs);
    expect(bcs[0]!.timestamp).toBe(2);
  });
});

describe('contextSections', () => {
  it('lists each context with its fields', () => {
    expect(
      contextSections({ os: { name: 'Android', version: '14' }, device: { class: 'phone' }, empty: {}, x: 'y' })
    ).toEqual([
      { name: 'device', fields: [['class', 'phone']] },
      { name: 'os', fields: [['name', 'Android'], ['version', '14']] },
      { name: 'x', fields: [['value', 'y']] }
    ]);
  });

  it('is empty without contexts', () => {
    expect(contextSections(null)).toEqual([]);
    expect(contextSections(undefined)).toEqual([]);
  });
});

describe('symbolicationJson', () => {
  it('is the event payload, which tool/symbolize.dart reads', () => {
    const stored: StoredEvent = {
      event_id: 'e1',
      received_at: '2026-11-05T00:00:00Z',
      payload: { extra: { dart_stack: STACK } }
    };
    expect(JSON.parse(symbolicationJson(stored))).toEqual({ extra: { dart_stack: STACK } });
  });
});

describe('eventFileName', () => {
  it('names the file by event id, keeping only safe characters', () => {
    expect(eventFileName('0f3a5b7c9d1e4f3a')).toBe('errex-event-0f3a5b7c9d1e4f3a.json');
    expect(eventFileName('../../x y')).toBe('errex-event-xy.json');
  });
});
