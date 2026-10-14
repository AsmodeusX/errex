<script lang="ts">
  import { cn } from '$lib/utils';

  type Props = { text: string; class?: string };
  let { text, class: className }: Props = $props();
</script>

<pre
  class={cn(
    'bg-muted/40 overflow-x-auto rounded-md p-2 font-mono text-[11px] leading-relaxed break-all whitespace-pre-wrap',
    className
  )}>{text}</pre>
