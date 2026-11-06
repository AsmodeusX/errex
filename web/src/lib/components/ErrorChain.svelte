<script lang="ts">
  import { Copy, ListTree } from 'lucide-svelte';
  import { copyText } from '$lib/clipboard';
  import { Badge } from '$lib/components/ui/badge';
  import { Button } from '$lib/components/ui/button';
  import { CodeBlock } from '$lib/components/ui/code-block';
  import { Collapsible } from '$lib/components/ui/collapsible';
  import { causeChain, primaryDartStack } from '$lib/eventDetail';
  import type { EventPayload } from '$lib/types';

  // The app's error as it sends it: the primary exception with its
  // obfuscated Dart stack (`extra.dart_stack`), then the wrapped causes
  // outer to inner (`extra.causes`). Stacks are shown and copied verbatim;
  // they are symbolicated offline, with symbols that never reach errex.
  type Props = {
    payload: EventPayload;
    type: string | null;
    message: string | null;
  };
  let { payload, type, message }: Props = $props();

  const primary = $derived(primaryDartStack(payload));
  const causes = $derived(causeChain(payload));

  let primaryOpen = $state(true);
</script>

{#snippet entry(label: string, t: string | null, m: string | null, stack: string | null)}
  <div class="flex flex-col gap-1.5 pt-1 pb-2 pl-4">
    <table class="w-full border-collapse font-mono text-[12px]">
      <tbody>
        <tr>
          <td class="text-muted-foreground w-20 py-0.5 pr-3 align-top">type</td>
          <td class="py-0.5 align-top break-all">{t ?? '—'}</td>
        </tr>
        <tr>
          <td class="text-muted-foreground w-20 py-0.5 pr-3 align-top">message</td>
          <td class="py-0.5 align-top break-words">{m ?? '—'}</td>
        </tr>
      </tbody>
    </table>
    {#if stack}
      <CodeBlock text={stack} class="max-h-96 overflow-y-auto" />
      <div>
        <Button variant="outline" size="sm" onclick={() => copyText(stack, `${label} stack`)}>
          <Copy class="h-3.5 w-3.5" />
          Copy stack for symbolication
        </Button>
      </div>
    {:else}
      <p class="text-muted-foreground text-[12px]">No Dart stack.</p>
    {/if}
  </div>
{/snippet}

<section class="px-3 py-2">
  <h2
    class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
  >
    <ListTree class="h-3.5 w-3.5" />
    Error and causes
    <span class="ml-1 text-muted-foreground/70 normal-case tracking-normal">
      {causes.length === 1 ? '1 cause' : `${causes.length} causes`}
    </span>
  </h2>
  {#if primary.buildId}
    <p class="text-muted-foreground mt-1 font-mono text-[11px]">
      build_id <span class="text-foreground">{primary.buildId}</span>
    </p>
  {/if}

  <ol class="mt-2 flex flex-col gap-1">
    <li>
      <Collapsible bind:open={primaryOpen} triggerClass="px-1 py-0.5 gap-2 text-[12px]">
        {#snippet header()}
          <Badge variant="destructive" class="shrink-0">primary</Badge>
          <span class="shrink-0 font-mono font-medium">{type ?? '<no type>'}</span>
          <span class="min-w-0 truncate text-muted-foreground">{message ?? ''}</span>
        {/snippet}
        {@render entry('Primary', type, message, primary.stack)}
      </Collapsible>
    </li>
    {#each causes as cause, i (i)}
      <li>
        <Collapsible triggerClass="px-1 py-0.5 gap-2 text-[12px]">
          {#snippet header()}
            <Badge variant="secondary" class="shrink-0">cause {i + 1}</Badge>
            <span class="shrink-0 font-mono font-medium">{cause.type ?? '<no type>'}</span>
            {#if cause.message && cause.message !== cause.type}
              <span class="min-w-0 truncate text-muted-foreground">{cause.message}</span>
            {/if}
          {/snippet}
          {@render entry(`Cause ${i + 1}`, cause.type, cause.message, cause.stack)}
        </Collapsible>
      </li>
    {/each}
  </ol>
</section>
