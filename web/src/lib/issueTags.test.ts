import { describe, expect, it } from 'vitest';
import { issueSummaryTags, matchesTagFilters } from './issueTags';

describe('issueSummaryTags', () => {
  it('returns the error code and context of the latest event, in that order', () => {
    expect(
      issueSummaryTags({ 'error.context': 'flash_plan', 'error.code': 'state_error', os: 'android' })
    ).toEqual([
      ['error.code', 'state_error'],
      ['error.context', 'flash_plan']
    ]);
  });

  it('skips the ones that are missing or empty', () => {
    expect(issueSummaryTags({ 'error.context': 'licence', 'error.code': '' })).toEqual([
      ['error.context', 'licence']
    ]);
    expect(issueSummaryTags(undefined)).toEqual([]);
    expect(issueSummaryTags(null)).toEqual([]);
  });
});

describe('matchesTagFilters', () => {
  const tags = { 'error.code': 'state_error', 'error.context': 'flash_plan', 'ecu.release': '1.0.11' };

  it('passes everything without filters', () => {
    expect(matchesTagFilters(undefined, [])).toBe(true);
  });

  it('matches a key and any of its values, ignoring case', () => {
    expect(matchesTagFilters(tags, [{ key: 'error.code', values: ['state_error'] }])).toBe(true);
    expect(matchesTagFilters(tags, [{ key: 'Error.Code', values: ['format', 'STATE_ERROR'] }])).toBe(true);
    expect(matchesTagFilters(tags, [{ key: 'error.code', values: ['format'] }])).toBe(false);
  });

  it('needs every filter to hold', () => {
    expect(
      matchesTagFilters(tags, [
        { key: 'error.code', values: ['state_error'] },
        { key: 'error.context', values: ['licence'] }
      ])
    ).toBe(false);
  });

  it('takes * as a wildcard', () => {
    expect(matchesTagFilters(tags, [{ key: 'error.context', values: ['flash*'] }])).toBe(true);
    expect(matchesTagFilters(tags, [{ key: 'ecu.release', values: ['1.0.*'] }])).toBe(true);
    expect(matchesTagFilters(tags, [{ key: 'ecu.release', values: ['1.1*'] }])).toBe(false);
  });

  it('matches a bare value against every tag', () => {
    expect(matchesTagFilters(tags, [{ key: null, values: ['flash_plan'] }])).toBe(true);
    expect(matchesTagFilters(tags, [{ key: null, values: ['nothing'] }])).toBe(false);
  });

  it('never matches an issue without tags', () => {
    expect(matchesTagFilters(undefined, [{ key: null, values: ['x'] }])).toBe(false);
  });
});
