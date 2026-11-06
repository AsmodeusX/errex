// Copy and save from the event view. Both stay in the browser: the clipboard
// and a local file, never a network request.

import { toast } from './toast.svelte';

/** Copy `text`, then toast `<label> copied` (or the failure). Never throws. */
export async function copyText(text: string, label: string): Promise<void> {
  try {
    if (!navigator.clipboard) throw new Error('no clipboard');
    await navigator.clipboard.writeText(text);
    toast.success(`${label} copied`, {
      description: `${text.length.toLocaleString()} chars`
    });
  } catch {
    toast.error('Could not copy');
  }
}

/** Save `text` as a JSON file named `name` through the browser's download. */
export function downloadText(name: string, text: string): void {
  const url = URL.createObjectURL(new Blob([text], { type: 'application/json' }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.rel = 'noopener';
  a.click();
  // Freed on the next tick: a synchronous revoke can cancel the download.
  setTimeout(() => URL.revokeObjectURL(url), 0);
}
