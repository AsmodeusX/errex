import { describe, expect, it } from 'vitest';
import { fireEvent, render, screen } from '@testing-library/svelte';
import ErrorChain from './ErrorChain.svelte';

const STACK =
  "build_id: '0f3a5b7c9d1e2f3a4152c37485e60718'\n" +
  '#00 abs 000000750a2f5c1f virt 00000000002f5c1f _kDartIsolateSnapshotInstructions+0x2f5c1f';
const HOSTILE = '<img src=x onerror="alert(1)">';

function renderChain() {
  return render(ErrorChain, {
    props: {
      payload: {
        extra: {
          dart_stack: STACK,
          causes: [
            { type: 'flash_failure', message: 'flash_failure', dart_stack: `${STACK}\n#01 abs 1` },
            { type: 'host_refusal', message: HOSTILE, dart_stack: '' }
          ]
        }
      },
      type: 'state_error',
      message: HOSTILE
    }
  });
}

describe('ErrorChain', () => {
  it('shows the primary error open, with its stack and build id', () => {
    const { container } = renderChain();
    expect(screen.getByText('2 causes')).toBeInTheDocument();
    expect(screen.getByText('0f3a5b7c9d1e2f3a4152c37485e60718')).toBeInTheDocument();
    const pres = container.querySelectorAll('pre');
    expect(pres).toHaveLength(1);
    expect(pres[0]!.textContent).toBe(STACK);
  });

  it('opens a cause on click, showing its type, message and stack', async () => {
    const { container } = renderChain();
    await fireEvent.click(screen.getByText('cause 1'));
    const pres = [...container.querySelectorAll('pre')].map((p) => p.textContent);
    expect(pres).toContain(`${STACK}\n#01 abs 1`);
    await fireEvent.click(screen.getByText('cause 2'));
    expect(screen.getByText('No Dart stack.')).toBeInTheDocument();
  });

  it('renders untrusted strings as text, never as markup', async () => {
    const { container } = renderChain();
    await fireEvent.click(screen.getByText('cause 2'));
    expect(container.querySelector('img')).toBeNull();
    expect(screen.getAllByText(HOSTILE).length).toBeGreaterThan(0);
  });
});
