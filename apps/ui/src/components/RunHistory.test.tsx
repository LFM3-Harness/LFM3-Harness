import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { RunHistory } from './RunHistory';
import {
  INTERRUPTED_SNAPSHOT,
  RETRIED_SNAPSHOT,
  RUN_ID,
  runRecord,
} from '../testFixtures';

const PASSED = runRecord();
const INTERRUPTED = runRecord({
  id: INTERRUPTED_SNAPSHOT.run.id,
  status: 'interrupted',
  goal: 'Interrupted while writing the summary',
});
const RETRIED = runRecord({
  id: RETRIED_SNAPSHOT.run.id,
  status: 'incomplete',
  parent_run_id: 'ffffffff000000001111111122222222',
});

describe('RunHistory', () => {
  it('keeps every terminal state distinguishable', () => {
    render(
      <RunHistory
        runs={[PASSED, INTERRUPTED, RETRIED]}
        activeRunId={null}
        onSelect={vi.fn()}
        onRetry={vi.fn()}
      />,
    );

    expect(screen.getByText('Passed')).toHaveClass('status-passed');
    expect(screen.getByText('Interrupted')).toHaveClass('status-interrupted');
    expect(screen.getByText('Incomplete')).toHaveClass('status-incomplete');
    expect(screen.getByText('Interrupted')).not.toHaveClass('status-passed');
  });

  it('links a passed run to its final evidence', async () => {
    const user = userEvent.setup();
    const onSelect = vi.fn();
    render(
      <RunHistory
        runs={[PASSED]}
        activeRunId={null}
        onSelect={onSelect}
        onRetry={vi.fn()}
      />,
    );

    await user.click(screen.getByRole('button', { name: /final evidence for d1e2f3a4/i }));

    expect(onSelect).toHaveBeenCalledWith(RUN_ID);
  });

  it('shows the retry parent link and retries failed history', async () => {
    const user = userEvent.setup();
    const onRetry = vi.fn();
    render(
      <RunHistory
        runs={[RETRIED]}
        activeRunId={null}
        onSelect={vi.fn()}
        onRetry={onRetry}
      />,
    );

    expect(screen.getByText(/retry of ffffffff/i)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /retry run aa11bb22/i }));

    expect(onRetry).toHaveBeenCalledWith(RETRIED.id);
  });
});
