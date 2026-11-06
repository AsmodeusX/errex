<script lang="ts">
  import { Tag } from 'lucide-svelte';
  import { dedupTags, groupTags } from '$lib/eventDetail';

  type Props = { tags: Record<string, string> };
  let { tags }: Props = $props();

  const groups = $derived(groupTags(dedupTags(tags)));
</script>

<section class="px-3 py-2">
  <h2
    class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
  >
    <Tag class="h-3.5 w-3.5" />
    Tags
  </h2>
  {#if groups.length === 0}
    <p class="text-muted-foreground mt-1 text-[12px]">No tags on this event.</p>
  {:else}
    <table class="mt-2 w-full border-collapse font-mono text-[12px]">
      {#each groups as group (group.id)}
        <tbody>
          <tr>
            <th
              colspan="2"
              scope="colgroup"
              class="pt-2 pb-0.5 text-left text-[10px] font-semibold uppercase tracking-wider text-muted-foreground/80"
            >
              {group.label}
            </th>
          </tr>
          {#each group.entries as [k, v] (k)}
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
