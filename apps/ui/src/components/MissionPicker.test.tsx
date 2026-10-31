import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { MissionPicker } from './MissionPicker';
import { DESK_MISSION, SCREEN_MISSION } from '../testFixtures';

function picker(overrides: Partial<Parameters<typeof MissionPicker>[0]> = {}) {
  const props = {
    missions: [DESK_MISSION, SCREEN_MISSION],
    selected: 'desk_task',
    inputs: {},
    onSelect: vi.fn(),
    onInputChange: vi.fn(),
    onChooseFile: vi.fn(),
    onStart: vi.fn(),
    ...overrides,
  };
  render(<MissionPicker {...props} />);
  return props;
}

describe('MissionPicker', () => {
  it('shows only the selected mission inputs', () => {
    picker({ selected: 'screen_qa' });

    expect(screen.getByRole('button', { name: /choose screenshot/i })).toBeInTheDocument();
    expect(screen.getByLabelText('Question')).toBeInTheDocument();
    expect(screen.queryByLabelText('Goal')).not.toBeInTheDocument();
    expect(screen.getByRole('button', { name: /create run/i })).toBeDisabled();
  });

  it('starts a run with the typed mission-specific input', async () => {
    const user = userEvent.setup();
    const props = picker({ inputs: { goal: 'Summarize the handover' } });

    expect(screen.getByLabelText('Goal')).toHaveValue('Summarize the handover');
    await user.click(screen.getByRole('button', { name: /create run/i }));

    expect(props.onStart).toHaveBeenCalledTimes(1);
  });

  it('keeps required string inputs blocking until provided', () => {
    picker({ inputs: {} });

    expect(screen.getByRole('button', { name: /create run/i })).toBeDisabled();
    expect(screen.getByText(/required input missing/i)).toBeInTheDocument();
  });

  it('selects a different mission through its card', async () => {
    const user = userEvent.setup();
    const props = picker();

    await user.click(screen.getByRole('button', { name: /screen question/i }));

    expect(props.onSelect).toHaveBeenCalledWith('screen_qa');
  });

  it('labels a human-review-only mission and refuses to start it', () => {
    const props = picker({
      selected: 'screen_qa',
      inputs: { screenshot: 'shot.png', question: 'Is there an error banner?' },
    });

    expect(screen.getAllByText(/human review only/i).length).toBeGreaterThan(0);
    expect(screen.getByRole('status')).toHaveTextContent(/cannot start/i);
    expect(screen.getByRole('status')).toHaveTextContent(/holdout qualification/i);
    const start = screen.getByRole('button', { name: /create run/i });
    expect(start).toBeDisabled();
    expect(props.onStart).not.toHaveBeenCalled();
  });
});
