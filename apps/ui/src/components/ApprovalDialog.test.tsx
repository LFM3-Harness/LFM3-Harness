import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { ApprovalDialog, type ApprovalRequest } from './ApprovalDialog';

const REQUEST: ApprovalRequest = {
  id: 'approval-1',
  run_id: 'd1e2f3a4b5c6d7e8f90123456789abcd',
  step_index: 3,
  tool: 'write_file',
  summary: 'Overwrite out/report.md',
  detail: 'The step replaces an existing report with 812 bytes of new content.',
  risk: 'medium',
  expires_at: null,
};

describe('ApprovalDialog', () => {
  it('allows exactly the pending approval', async () => {
    const user = userEvent.setup();
    const onDecide = vi.fn();
    render(<ApprovalDialog request={REQUEST} onDecide={onDecide} />);

    expect(screen.getByRole('dialog')).toHaveAccessibleName(/approval requested/i);
    expect(screen.getByText(/overwrite out\/report\.md/i)).toBeInTheDocument();
    expect(screen.getByText(/medium risk/i)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: /allow once/i }));

    expect(onDecide).toHaveBeenCalledWith('approval-1', true);
  });

  it('denies without allowing', async () => {
    const user = userEvent.setup();
    const onDecide = vi.fn();
    render(<ApprovalDialog request={REQUEST} onDecide={onDecide} />);

    await user.click(screen.getByRole('button', { name: /deny/i }));

    expect(onDecide).toHaveBeenCalledWith('approval-1', false);
  });

  it('renders nothing when no approval is pending', () => {
    const { container } = render(<ApprovalDialog request={null} onDecide={vi.fn()} />);

    expect(container).toBeEmptyDOMElement();
  });
});
