<script lang="ts">
  import { Package } from 'lucide-svelte';
  import { api } from '$lib/api';
  import { Badge } from '$lib/components/ui/badge';
  import { Button } from '$lib/components/ui/button';
  import { resolveIn } from '$lib/issueOps';
  import { canResolveIn, releaseFacts } from '$lib/releases';
  import type { Issue, IssueRelease } from '$lib/types';
  import { relativeTime } from '$lib/utils';

  type Props = { issue: Issue };
  let { issue }: Props = $props();

  let rows = $state<IssueRelease[]>([]);
  let failed = $state(false);

  const facts = $derived(releaseFacts(issue));

  // Refetch when another event lands (the WS update bumps event_count) or
  // the selection moves; a late answer for a previous issue is dropped.
  $effect(() => {
    const id = issue.id;
    void issue.event_count;
    let current = true;
    api.issueReleases(id).then(
      (r) => {
        if (!current) return;
        rows = r ?? [];
        failed = false;
      },
      () => {
        if (current) failed = true;
      }
    );
    return () => {
      current = false;
    };
  });
</script>

<section class="px-3 py-2">
  <h2
    class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
  >
    <Package class="h-3.5 w-3.5" />
    Releases
  </h2>
  {#if facts.length > 0}
    <p class="mt-1 flex flex-wrap gap-x-3 gap-y-0.5 text-[12px]">
      {#each facts as fact (fact.label)}
        <span>
          <span class="text-muted-foreground">{fact.label}</span>
          <span class="font-mono">{fact.value}</span>
        </span>
      {/each}
    </p>
  {/if}
  {#if failed}
    <p class="text-muted-foreground mt-1 text-[12px]">Could not load the releases.</p>
  {:else if rows.length === 0}
    <p class="text-muted-foreground mt-1 text-[12px]">No release reported this issue.</p>
  {:else}
    <table class="mt-2 w-full border-collapse text-[12px]">
      <thead class="text-muted-foreground text-left">
        <tr>
          <th class="py-1 pr-2 font-normal">Release</th>
          <th class="py-1 pr-2 text-right font-normal">Events</th>
          <th class="py-1 pr-2 font-normal">First</th>
          <th class="py-1 pr-2 font-normal">Last</th>
          <th class="py-1"></th>
        </tr>
      </thead>
      <tbody>
        {#each rows as row (row.release)}
          <tr class="border-t border-border">
            <td class="py-1 pr-2 font-mono break-all">
              {row.release}
              {#if issue.status === 'resolved' && issue.resolved_in_release === row.release}
                <Badge variant="outline" class="ml-1 px-1.5 py-0 text-[10px]">resolved in</Badge>
              {/if}
            </td>
            <td class="py-1 pr-2 text-right tabular-nums">{row.event_count}</td>
            <td class="py-1 pr-2 whitespace-nowrap">{relativeTime(row.first_seen)}</td>
            <td class="py-1 pr-2 whitespace-nowrap">{relativeTime(row.last_seen)}</td>
            <td class="py-1 text-right">
              {#if canResolveIn(issue, row.release)}
                <Button
                  variant="link"
                  class="h-auto px-0 text-[12px]"
                  onclick={() => void resolveIn(issue, row.release)}
                >
                  Resolve in
                </Button>
              {/if}
            </td>
          </tr>
        {/each}
      </tbody>
    </table>
  {/if}
</section>
