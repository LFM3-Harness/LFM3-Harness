import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { RunTimeline } from './RunTimeline';
import {
  INTERRUPTED_EVENTS,
  INTERRUPTED_SNAPSHOT,
  PASSED_SNAPSHOT,
  RETRIED_EVENTS,
  RETRIED_SNAPSHOT,
  RUN_ID,
} from '../testFixtures';
import type { RunSnapshot } from '../bridge/types';

describe('RunTimeline', () => {
  it('names the failed criterion and keeps strategy history', () => {
    render(
      <RunTimeline
        snapshot={RETRIED_SNAPSHOT}
        events={RETRIED_EVENTS}
        onCancel={vi.fn()}
        onRetry={vi.fn()}
      />,
    );

    expect(screen.getByText('Incomplete')).toBeInTheDocument();
    expect(screen.getByText(RETRIED_SNAPSHOT.steps[0].criterion)).toBeInTheDocument();
    expect(screen.getByText(/does not contain "owner"/)).toBeInTheDocument();
    expect(screen.getByText(/strategy 2/i)).toBeInTheDocument();
    expect(screen.getByText(/recovery: replan/i)).toBeInTheDocument();
    expect(screen.getByText(/budget_exhausted/i)).toBeInTheDocument();
  });

  it('offers cancellation while a run is active', async () => {
    const user = userEvent.setup();
    const onCancel = vi.fn();
    const running: RunSnapshot = {
      ...PASSED_SNAPSHOT,
      run: { ...PASSED_SNAPSHOT.run, status: 'running' },
    };
    render(<RunTimeline snapshot={running} events={[]} onCancel={onCancel} />);

    await user.click(screen.getByRole('button', { name: /cancel run/i }));

    expect(onCancel).toHaveBeenCalledWith(RUN_ID);
  });

  it('disables cancellation once a run is terminal', () => {
    render(
      <RunTimeline snapshot={PASSED_SNAPSHOT} events={[]} onCancel={vi.fn()} />,
    );

    expect(screen.getByRole('button', { name: /cancel run/i })).toBeDisabled();
  });

  it('never presents an interrupted run as a success', () => {
    render(
      <RunTimeline
        snapshot={INTERRUPTED_SNAPSHOT}
        events={INTERRUPTED_EVENTS}
        onCancel={vi.fn()}
        onRetry={vi.fn()}
      />,
    );

    expect(screen.getByText('Interrupted')).toBeInTheDocument();
    expect(screen.queryByText('Passed')).not.toBeInTheDocument();
    expect(screen.getByText(/engine exited during attempt execution/i)).toBeInTheDocument();
    expect(screen.getByText(/not a success/i)).toBeInTheDocument();
  });

  it('retries a terminal run as a new linked run', async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    render(
      <RunTimeline
        snapshot={RETRIED_SNAPSHOT}
        events={RETRIED_EVENTS}
        onCancel={vi.fn()}
        onRetry={onRetry}
      />,
    );

    await user.click(screen.getByRole('button', { name: /retry run/i }));

    expect(onRetry).toHaveBeenCalledWith(RETRIED_SNAPSHOT.run.id);
  });
});
