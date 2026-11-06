import { afterEach, describe, expect, it, vi } from 'vitest';
import { copyText, downloadText } from './clipboard';
import { toast } from './toast.svelte';

afterEach(() => {
  vi.restoreAllMocks();
  toast.list = [];
});

describe('copyText', () => {
  it('writes the text and says what was copied', async () => {
    const writeText = vi.fn().mockResolvedValue(undefined);
    vi.stubGlobal('navigator', { clipboard: { writeText } });
    await copyText('stack text', 'Stack');
    expect(writeText).toHaveBeenCalledWith('stack text');
    expect(toast.list.at(-1)?.message).toBe('Stack copied');
    vi.unstubAllGlobals();
  });

  it('reports a failure without throwing', async () => {
    vi.stubGlobal('navigator', { clipboard: { writeText: vi.fn().mockRejectedValue(new Error('denied')) } });
    await copyText('x', 'Stack');
    expect(toast.list.at(-1)?.variant).toBe('error');
    vi.stubGlobal('navigator', {});
    await copyText('x', 'Stack');
    expect(toast.list.at(-1)?.variant).toBe('error');
    vi.unstubAllGlobals();
  });
});

describe('downloadText', () => {
  it('saves the text as a local file and frees the object URL', () => {
    const create = vi.fn().mockReturnValue('blob:local/1');
    const revoke = vi.fn();
    vi.stubGlobal('URL', { ...URL, createObjectURL: create, revokeObjectURL: revoke });
    const click = vi.spyOn(HTMLAnchorElement.prototype, 'click').mockImplementation(() => {});
    vi.useFakeTimers();
    downloadText('errex-event-1.json', '{"a":1}');
    const blob = create.mock.calls[0]![0] as Blob;
    expect(blob.type).toBe('application/json');
    expect(click).toHaveBeenCalledOnce();
    const anchor = click.mock.instances[0] as unknown as HTMLAnchorElement;
    expect(anchor.download).toBe('errex-event-1.json');
    expect(anchor.href).toBe('blob:local/1');
    expect(revoke).not.toHaveBeenCalled();
    vi.runAllTimers();
    expect(revoke).toHaveBeenCalledWith('blob:local/1');
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });
});
