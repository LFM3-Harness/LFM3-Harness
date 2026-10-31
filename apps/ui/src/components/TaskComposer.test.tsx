import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { TaskComposer } from './TaskComposer';
import type { TaskRoute } from '../bridge/types';

const ROUTE: TaskRoute = {
  roles: [
    { role: 'plan', model: 'text', capability: 'text', used: true, reason: null },
    { role: 'act', model: 'text', capability: 'text', used: true, reason: null },
    { role: 'observe', model: 'vision', capability: 'vision', used: false, reason: null },
  ],
  images: [],
  audio: [],
  tools: ['read_file', 'write_file'],
};

function composer(overrides: Partial<Parameters<typeof TaskComposer>[0]> = {}) {
  const props = {
    route: ROUTE,
    onStart: vi.fn(),
    ...overrides,
  };
  render(<TaskComposer {...props} />);
  return props;
}

describe('TaskComposer', () => {
  it('blocks starting until a job is stated', () => {
    composer();

    expect(screen.getByRole('button', { name: /start job/i })).toBeDisabled();
    expect(screen.getByLabelText(/job budget/i)).toHaveValue(30);
    expect(screen.getByLabelText(/maximum turns/i)).toHaveValue(24);
  });

  it('starts a job with the stated goal, output, budget and turn cap', async () => {
    const user = userEvent.setup();
    const props = composer();

    await user.type(screen.getByLabelText(/^job$/i), 'Rewrite every note into out/report.md');
    await user.type(screen.getByLabelText(/required output/i), 'out/report.md');
    await user.click(screen.getByRole('button', { name: /start job/i }));

    expect(props.onStart).toHaveBeenCalledWith(
      'Rewrite every note into out/report.md',
      'out/report.md',
      30,
      24,
    );
  });

  it('passes the chosen job budget and turn cap through', async () => {
    const user = userEvent.setup();
    const props = composer();

    await user.type(screen.getByLabelText(/^job$/i), 'Summarise the notes');
    await user.clear(screen.getByLabelText(/job budget/i));
    await user.type(screen.getByLabelText(/job budget/i), '45');
    await user.clear(screen.getByLabelText(/maximum turns/i));
    await user.type(screen.getByLabelText(/maximum turns/i), '6');
    await user.click(screen.getByRole('button', { name: /start job/i }));

    expect(props.onStart).toHaveBeenCalledWith('Summarise the notes', '', 45, 6);
  });

  it('bounds the budget and turn fields to usable ranges', () => {
    composer();

    expect(screen.getByLabelText(/job budget/i)).toHaveAttribute('min', '1');
    expect(screen.getByLabelText(/maximum turns/i)).toHaveAttribute('min', '1');
    expect(screen.getByLabelText(/maximum turns/i)).toHaveAttribute('max', '64');
  });

  it('refuses to start a job in demo mode', () => {
    composer({ demo: true });

    expect(screen.getByRole('button', { name: /desktop app only/i })).toBeDisabled();
    expect(screen.getByText(/start a job in the desktop app/i)).toBeVisible();
  });

  it('shows which local models the goal is routed to', () => {
    composer();

    const routing = screen.getByLabelText(/model routing/i);
    expect(routing).toHaveTextContent('plan · text');
    expect(routing).toHaveTextContent('observe · vision (idle)');
  });
});
