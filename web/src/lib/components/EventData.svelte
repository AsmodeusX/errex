<script lang="ts">
  import { Database } from 'lucide-svelte';
  import { CodeBlock } from '$lib/components/ui/code-block';
  import { Collapsible } from '$lib/components/ui/collapsible';
  import { additionalData, formatData } from '$lib/eventDetail';
  import type { EventPayload } from '$lib/types';

  type Props = { payload: EventPayload | null };
  let { payload }: Props = $props();

  const entries = $derived(payload ? additionalData(payload) : []);
</script>

<section class="px-3 py-2">
  <h2
    class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
  >
    <Database class="h-3.5 w-3.5" />
    Additional data
  </h2>
  {#if entries.length === 0}
    <p class="text-muted-foreground mt-1 text-[12px]">No additional data on this event.</p>
  {:else}
    <ul class="mt-2 flex flex-col gap-1">
      {#each entries as entry (entry.key)}
        <li>
          <Collapsible triggerClass="px-1 py-0.5 font-mono text-[12px] gap-2" contentClass="pt-1">
            {#snippet header()}
              <span>{entry.key}</span>
            {/snippet}
            <CodeBlock text={formatData(entry.value)} />
          </Collapsible>
        </li>
      {/each}
    </ul>
  {/if}
</section>
