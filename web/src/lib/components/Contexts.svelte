<script lang="ts">
  import { MonitorSmartphone } from 'lucide-svelte';
  import { contextSections } from '$lib/eventDetail';

  type Props = { contexts: Record<string, unknown> | null | undefined };
  let { contexts }: Props = $props();

  const sections = $derived(contextSections(contexts));
</script>

<section class="px-3 py-2">
  <h2
    class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
  >
    <MonitorSmartphone class="h-3.5 w-3.5" />
    Contexts
  </h2>
  {#if sections.length === 0}
    <p class="text-muted-foreground mt-1 text-[12px]">No contexts on this event.</p>
  {:else}
    <table class="mt-2 w-full border-collapse font-mono text-[12px]">
      {#each sections as section (section.name)}
        <tbody>
          <tr>
            <th
              colspan="2"
              scope="colgroup"
              class="pt-2 pb-0.5 text-left text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80"
            >
              {section.name}
            </th>
          </tr>
          {#each section.fields as [k, v] (k)}
            <tr class="border-b border-border/30 last:border-0">
              <td class="text-muted-foreground w-1/3 py-0.5 pr-3 align-top break-all">{k}</td>
              <td class="py-0.5 align-top break-all">{v}</td>
            </tr>
          {/each}
        </tbody>
      {/each}
    </table>
  {/if}
</section>
