<script lang="ts">
  import { Footprints } from 'lucide-svelte';
  import { Badge } from '$lib/components/ui/badge';
  import { Collapsible } from '$lib/components/ui/collapsible';
  import {
    breadcrumbData,
    breadcrumbRelativeTime,
    breadcrumbsInTimeOrder,
    parseSentryTimestamp
  } from '$lib/eventDetail';
  import type { Breadcrumb } from '$lib/types';
  import { cn } from '$lib/utils';

  type Props = { breadcrumbs: Breadcrumb[]; crashTimestamp?: string | null };
  let { breadcrumbs, crashTimestamp = null }: Props = $props();

  let open = $state(true);

  const ordered = $derived(breadcrumbsInTimeOrder(breadcrumbs));

  // Anchor index: the breadcrumb closest to (and ≤) the crash timestamp.
  // That row is the "last thing the user did before it broke", which we
  // subtly highlight so the eye lands on it without hunting.
  // Sentry JS SDKs emit timestamps as Unix-seconds floats (not ISO
  // strings), so route both through `parseSentryTimestamp` rather than
  // bare `Date.parse` — see eventDetail.ts.
  const anchorIndex = $derived.by(() => {
    if (!crashTimestamp || ordered.length === 0) return -1;
    const crash = parseSentryTimestamp(crashTimestamp);
    if (!Number.isFinite(crash)) return -1;
    let best = -1;
    let bestDelta = Number.POSITIVE_INFINITY;
    ordered.forEach((bc, i) => {
      const t = parseSentryTimestamp(bc.timestamp);
      if (!Number.isFinite(t) || t > crash) return;
      const d = crash - t;
      if (d < bestDelta) {
        bestDelta = d;
        best = i;
      }
    });
    return best;
  });

  type Variant = 'info' | 'warning' | 'destructive' | 'outline' | 'secondary' | 'success';

  // A crumb's badge: its level when that is a problem, else its category,
  // so a page change, a user action and a server answer read apart.
  function badgeVariant(level: string | null, category: string | null): Variant {
    if (level === 'error' || level === 'fatal') return 'destructive';
    if (level === 'warning') return 'warning';
    switch (category) {
      case 'navigation':
        return 'info';
      case 'action':
        return 'success';
      case 'licensing':
      case 'flash':
        return 'warning';
      case 'adapter':
      case 'ecu':
      case 'car':
      case 'logging':
        return 'secondary';
      default:
        return 'outline';
    }
  }
</script>

<section>
  <Collapsible bind:open triggerClass="px-3 py-2 sticky top-0 bg-background">
    {#snippet header()}
      <h2
        class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
      >
        <Footprints class="h-3.5 w-3.5" />
        Breadcrumbs
        <span class="ml-1 text-muted-foreground/70 normal-case">({ordered.length})</span>
      </h2>
    {/snippet}

    {#if ordered.length === 0}
      <p class="text-muted-foreground px-3 pb-3 text-[12px]">No breadcrumbs for this event.</p>
    {:else}
      <ol class="flex flex-col">
        {#each ordered as bc, i (i)}
          {@const isAnchor = i === anchorIndex}
          {@const data = breadcrumbData(bc)}
          <li
            class={cn(
              'flex items-start gap-2 border-t border-border/40 px-3 py-1.5 first:border-t-0',
              isAnchor && 'border-l-2 border-l-amber-500 bg-amber-500/5 pl-2.5'
            )}
          >
            <span
              class={cn(
                'shrink-0 font-mono text-[11px] tabular-nums',
                isAnchor ? 'text-amber-500 font-semibold' : 'text-muted-foreground'
              )}
              aria-label={isAnchor ? 'last breadcrumb before the crash' : undefined}
            >
              {crashTimestamp ? breadcrumbRelativeTime(crashTimestamp, bc.timestamp) : (bc.timestamp ?? '—')}
            </span>
            <Badge variant={badgeVariant(bc.level ?? null, bc.category ?? null)} class="shrink-0">
              {bc.category ?? 'event'}
            </Badge>
            <div class="flex min-w-0 flex-1 flex-col gap-0.5">
              <span class="break-words text-[12px]">
                {bc.message ?? ''}
                {#if bc.level && bc.level !== 'info'}
                  <span class="text-muted-foreground ml-1 font-mono text-[10px] uppercase">{bc.level}</span>
                {/if}
              </span>
              {#if data.length > 0}
                <dl class="flex flex-wrap gap-x-3 gap-y-0.5 font-mono text-[11px]">
                  {#each data as [k, v] (k)}
                    <div class="flex min-w-0 gap-1">
                      <dt class="text-muted-foreground">{k}</dt>
                      <dd class="break-all">{v}</dd>
                    </div>
                  {/each}
                </dl>
              {/if}
            </div>
          </li>
        {/each}
      </ol>
    {/if}
  </Collapsible>
</section>
