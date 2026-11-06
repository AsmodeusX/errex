<script lang="ts">
  import { Braces, Copy, Download } from 'lucide-svelte';
  import { copyText, downloadText } from '$lib/clipboard';
  import { Button } from '$lib/components/ui/button';
  import { CodeBlock } from '$lib/components/ui/code-block';
  import { Collapsible } from '$lib/components/ui/collapsible';
  import { eventFileName, symbolicationJson } from '$lib/eventDetail';
  import type { StoredEvent } from '$lib/types';

  // The stored event, whole. Copy and download stay in the browser; the
  // payload copy is what an offline symbolizer reads (`extra.dart_stack` at
  // the top level).
  type Props = { stored: StoredEvent };
  let { stored }: Props = $props();

  let open = $state(false);
  const json = $derived(JSON.stringify(stored, null, 2));
  const size = $derived(`${Math.max(1, Math.round(json.length / 1024)).toLocaleString()} KB`);
</script>

<section class="px-3 py-2">
  <Collapsible bind:open triggerClass="py-0.5 gap-1.5">
    {#snippet header()}
      <h2
        class="flex items-center gap-1.5 text-[12px] font-semibold uppercase tracking-wider text-muted-foreground"
      >
        <Braces class="h-3.5 w-3.5" />
        Raw event JSON
        <span class="ml-1 text-muted-foreground/70 normal-case tracking-normal">{size}</span>
      </h2>
    {/snippet}
    <div class="flex flex-col gap-2 pt-2">
      <div class="flex flex-wrap gap-1.5">
        <Button variant="outline" size="sm" onclick={() => copyText(json, 'Event JSON')}>
          <Copy class="h-3.5 w-3.5" />
          Copy JSON
        </Button>
        <Button
          variant="outline"
          size="sm"
          onclick={() => copyText(symbolicationJson(stored), 'Event payload')}
        >
          <Copy class="h-3.5 w-3.5" />
          Copy payload for symbolication
        </Button>
        <Button
          variant="outline"
          size="sm"
          onclick={() => downloadText(eventFileName(stored.event_id), symbolicationJson(stored))}
        >
          <Download class="h-3.5 w-3.5" />
          Download payload
        </Button>
      </div>
      <CodeBlock text={json} class="max-h-[32rem] overflow-y-auto" />
    </div>
  </Collapsible>
</section>
