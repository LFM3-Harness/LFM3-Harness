import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { AcceptanceReview } from './AcceptanceReview';
import { ACCEPTANCE, ACCEPTANCE_HASH } from '../testFixtures';

describe('AcceptanceReview', () => {
  it('renders the frozen plan, checks, paths, budgets, and full hash', () => {
    render(
      <AcceptanceReview
        acceptance={ACCEPTANCE}
        acceptanceHash={ACCEPTANCE_HASH}
        expectedHash={ACCEPTANCE_HASH}
        onApprove={vi.fn()}
        onReject={vi.fn()}
      />,
    );

    expect(screen.getByTestId('acceptance-hash')).toHaveTextContent(ACCEPTANCE_HASH);
    expect(screen.getByText(ACCEPTANCE.steps[0].criterion)).toBeInTheDocument();
    expect(screen.getByText(/content_matches/)).toBeInTheDocument();
    expect(screen.getByText('out/summary.md')).toBeInTheDocument();
    expect(screen.getByText(/max attempts 3/i)).toBeInTheDocument();
    expect(screen.getByText('write_file')).toBeInTheDocument();
  });

  it('approves with the displayed acceptance hash', async () => {
    const user = userEvent.setup();
    const onApprove = vi.fn();
    const onReject = vi.fn();
    render(
      <AcceptanceReview
        acceptance={ACCEPTANCE}
        acceptanceHash={ACCEPTANCE_HASH}
        expectedHash={ACCEPTANCE_HASH}
        onApprove={onApprove}
        onReject={onReject}
      />,
    );

    await user.click(screen.getByRole('button', { name: /approve plan/i }));

    expect(onApprove).toHaveBeenCalledWith(ACCEPTANCE_HASH);
    expect(onReject).not.toHaveBeenCalled();
  });

  it('rejects the plan without approving it', async () => {
    const user = userEvent.setup();
    const onApprove = vi.fn();
    const onReject = vi.fn();
    render(
      <AcceptanceReview
        acceptance={ACCEPTANCE}
        acceptanceHash={ACCEPTANCE_HASH}
        expectedHash={ACCEPTANCE_HASH}
        onApprove={onApprove}
        onReject={onReject}
      />,
    );

    await user.click(screen.getByRole('button', { name: /reject plan/i }));

    expect(onReject).toHaveBeenCalledTimes(1);
    expect(onApprove).not.toHaveBeenCalled();
  });

  it('blocks approval when the hash does not match the run', () => {
    render(
      <AcceptanceReview
        acceptance={ACCEPTANCE}
        acceptanceHash={ACCEPTANCE_HASH}
        expectedHash={'0'.repeat(64)}
        onApprove={vi.fn()}
        onReject={vi.fn()}
      />,
    );

    expect(screen.getByRole('alert')).toHaveTextContent(/hash does not match/i);
    expect(screen.getByRole('button', { name: /approve plan/i })).toBeDisabled();
  });
});
