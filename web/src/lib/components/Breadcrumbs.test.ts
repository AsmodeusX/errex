import { describe, expect, it } from 'vitest';
import { render, screen } from '@testing-library/svelte';
import Breadcrumbs from './Breadcrumbs.svelte';

describe('Breadcrumbs', () => {
  it('lists every crumb in time order with its category and data fields', () => {
    const { container } = render(Breadcrumbs, {
      props: {
        crashTimestamp: '100',
        breadcrumbs: [
          {
            timestamp: 90,
            category: 'licensing',
            message: 'Licensing activate: 409',
            level: 'info',
            data: { endpoint: 'activate', status: '409', code: 'bad_credentials', kind: 'input' }
          },
          {
            timestamp: 80,
            category: 'navigation',
            message: 'Opened flash',
            level: 'info',
            data: { page: 'flash', operation: 'push' }
          },
          { timestamp: 85, category: 'action', message: 'Started flash_plan', data: { action: 'flash_plan' } }
        ]
      }
    });
    const rows = [...container.querySelectorAll('li')].map((li) => li.textContent ?? '');
    expect(rows).toHaveLength(3);
    expect(rows[0]).toContain('Opened flash');
    expect(rows[1]).toContain('Started flash_plan');
    expect(rows[2]).toContain('Licensing activate: 409');
    for (const field of ['endpoint', 'activate', 'status', 'bad_credentials', 'kind', 'input']) {
      expect(rows[2]).toContain(field);
    }
    expect(rows[0]).toContain('operation');
    expect(screen.getByText('navigation')).toBeInTheDocument();
    expect(screen.getByText('licensing')).toBeInTheDocument();
  });

  it('renders data values as text, never as markup', () => {
    const { container } = render(Breadcrumbs, {
      props: { breadcrumbs: [{ category: 'action', message: 'm', data: { action: '<b>x</b>' } }] }
    });
    expect(container.querySelector('b')).toBeNull();
    expect(screen.getByText('<b>x</b>')).toBeInTheDocument();
  });
});
