import { describe, expect, it } from 'vitest';
import { canResolveIn, releaseFacts } from './releases';
import type { Issue } from './types';

function issue(over: Partial<Issue> = {}): Issue {
  return {
    id: 1,
    project: 'p',
    fingerprint: 'fp',
    title: 'T',
    culprit: null,
    level: null,
    status: 'unresolved',
    event_count: 1,
    first_seen: 'now',
    last_seen: 'now',
    ...over
  };
}

describe('releaseFacts', () => {
  it('is empty for an issue no release reported', () => {
    expect(releaseFacts(issue())).toEqual([]);
  });

  it('collapses releases that are the same', () => {
    expect(
      releaseFacts(issue({ first_release: 'a+1', newest_release: 'a+1', last_release: 'a+1' }))
    ).toEqual([{ label: 'First in', value: 'a+1' }]);
  });

  it('lists first, newest and last event when they differ', () => {
    expect(
      releaseFacts(issue({ first_release: 'a+2', newest_release: 'a+3', last_release: 'a+1' }))
    ).toEqual([
      { label: 'First in', value: 'a+2' },
      { label: 'Newest', value: 'a+3' },
      { label: 'Last event', value: 'a+1' }
    ]);
  });

  it('names the release a resolved issue is resolved in, or says there is none', () => {
    const facts = releaseFacts(
      issue({ status: 'resolved', first_release: 'a+1', resolved_in_release: 'a+2' })
    );
    expect(facts.at(-1)).toEqual({ label: 'Resolved in', value: 'a+2' });
    expect(releaseFacts(issue({ status: 'resolved' }))).toEqual([
      { label: 'Resolved in', value: 'no release' }
    ]);
  });
});

describe('canResolveIn', () => {
  it('offers every release except the one the issue is resolved in', () => {
    expect(canResolveIn(issue(), 'a+1')).toBe(true);
    const resolved = issue({ status: 'resolved', resolved_in_release: 'a+1' });
    expect(canResolveIn(resolved, 'a+1')).toBe(false);
    expect(canResolveIn(resolved, 'a+2')).toBe(true);
  });
});
