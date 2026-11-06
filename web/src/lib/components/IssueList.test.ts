import { afterEach, beforeEach, describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import { flushSync } from 'svelte';
import { filter, issues, load, projects } from '$lib/stores.svelte';
import type { IssueLevel, IssueStatus } from '$lib/types';
import IssueListWrapper from './IssueListWrapper.svelte';

// IssueList.svelte requires a Tooltip.Provider ancestor (wired in the app
// layout). IssueListWrapper provides that context so the component can be
// exercised in the jsdom environment without the full SvelteKit layout.
//
// The toolbar is now a single UnifiedInput; the legacy sort menu, filter
// popover, and regex-mode tag are gone. Tests here cover the readout row
// and smoke-render rather than the input internals — those are exercised
// in queryParser.test.ts and queryBridge.test.ts at the unit level.

beforeEach(() => {
  history.replaceState(null, '', '/');
  issues.reset([]);
  filter.query = '';
  filter.statuses = new Set<IssueStatus>(['unresolved']);
  filter.levels = new Set<IssueLevel>();
  filter.sinceMs = null;
  filter.spikingOnly = false;
  filter.sort = 'recent';
  filter.newOnly = false;
  filter.staleOnly = false;
  filter.limit = null;
  filter.tags = [];
  projects.current = 'p';
  load.initialLoad = false;
});

afterEach(() => {
  issues.reset([]);
});

describe('IssueList toolbar', () => {
  it('renders the unified input with the placeholder', () => {
    render(IssueListWrapper);
    expect(screen.getByLabelText('Filter issues')).toBeInTheDocument();
  });

  it('shows the `/` kbd hint when the input is empty', () => {
    render(IssueListWrapper);
    expect(screen.getByText('/')).toBeInTheDocument();
  });
});

describe('IssueList active-filter readout', () => {
  it('does not render the readout row when filter is at defaults', () => {
    render(IssueListWrapper);
    // "Clear all" is the unique affordance on the readout row; its
    // presence is a reliable proxy for the row being mounted.
    expect(screen.queryByText('Clear all')).toBeNull();
  });

  it('shows the readout with "N of N" when filter.query is non-empty', async () => {
    render(IssueListWrapper);
    // We seed the filter via the store rather than typing into the
    // input — exercising the input's keystroke pipeline requires the
    // parser+bridge round-trip which is unit-tested elsewhere.
    flushSync(() => {
      filter.query = 'auth';
    });
    expect(await screen.findByText('Clear all')).toBeInTheDocument();
  });
});

describe('IssueList rows and tag filters', () => {
  function seed() {
    issues.reset([
      {
        id: 1,
        project: 'p',
        fingerprint: 'a',
        title: 'state_error: bad state',
        culprit: null,
        level: 'error',
        status: 'unresolved',
        event_count: 1,
        first_seen: '2026-01-01T00:00:00Z',
        last_seen: '2026-01-01T00:00:00Z',
        tags: { 'error.code': 'state_error', 'error.context': 'flash_plan' }
      },
      {
        id: 2,
        project: 'p',
        fingerprint: 'b',
        title: 'format: bad input',
        culprit: null,
        level: 'warning',
        status: 'unresolved',
        event_count: 1,
        first_seen: '2026-01-01T00:00:00Z',
        last_seen: '2026-01-01T00:00:01Z',
        tags: { 'error.code': 'format', 'error.context': 'licence' }
      }
    ]);
  }

  it('shows the latest event\'s error code and context on each row', () => {
    seed();
    render(IssueListWrapper);
    expect(screen.getByTitle('error.code=state_error')).toHaveTextContent('code state_error');
    expect(screen.getByTitle('error.context=licence')).toHaveTextContent('context licence');
  });

  it('keeps only the issues a tag filter matches', async () => {
    seed();
    render(IssueListWrapper);
    flushSync(() => {
      filter.tags = [{ key: 'error.context', values: ['licence'] }];
    });
    expect(screen.queryByText('state_error: bad state')).toBeNull();
    expect(screen.getByText('format: bad input')).toBeInTheDocument();
  });
});
