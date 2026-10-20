// Release facts shown on the issue page. The daemon keeps one issue per
// defect across releases and records which releases reported it; a
// resolved issue also names the release it was resolved in.

import type { Issue } from './types';

export interface ReleaseFact {
  label: string;
  value: string;
}

/** The issue's release summary, in reading order. Omits what is unknown,
 *  except the resolved-in release of a resolved issue: "no release" there
 *  means any new event regresses it. */
export function releaseFacts(issue: Issue): ReleaseFact[] {
  const facts: ReleaseFact[] = [];
  if (issue.first_release) facts.push({ label: 'First in', value: issue.first_release });
  if (issue.newest_release && issue.newest_release !== issue.first_release) {
    facts.push({ label: 'Newest', value: issue.newest_release });
  }
  if (issue.last_release && issue.last_release !== issue.newest_release) {
    facts.push({ label: 'Last event', value: issue.last_release });
  }
  if (issue.status === 'resolved') {
    facts.push({ label: 'Resolved in', value: issue.resolved_in_release ?? 'no release' });
  }
  return facts;
}

/** Whether "Resolve in <release>" is offered on a row: not for the release
 *  the issue is already resolved in. */
export function canResolveIn(issue: Issue, release: string): boolean {
  return !(issue.status === 'resolved' && issue.resolved_in_release === release);
}
