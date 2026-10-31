import { render, screen } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import { describe, expect, it, vi } from 'vitest';

import { ModelDock } from './ModelDock';
import { HEALTH, MODELS } from '../testFixtures';

describe('ModelDock', () => {
  it('shows the pinned runtime and every model state', () => {
    render(<ModelDock models={MODELS} llama={HEALTH.llama} onRelease={vi.fn()} />);

    expect(screen.getByText(/build 11960/)).toBeInTheDocument();
    expect(screen.getByText('0.8.0-dev')).toBeInTheDocument();
    expect(screen.getByText(/verified pin/i)).toBeInTheDocument();
    expect(screen.getByText('lfm3-1.7b')).toBeInTheDocument();
    expect(screen.getByText('lfm3-vl-2b')).toBeInTheDocument();
    expect(screen.getByText('ready')).toBeInTheDocument();
    expect(screen.getByText('absent')).toBeInTheDocument();
  });

  it('releases only a loaded model', async () => {
    const user = userEvent.setup();
    const onRelease = vi.fn();
    render(<ModelDock models={MODELS} llama={HEALTH.llama} onRelease={onRelease} />);

    await user.click(screen.getByRole('button', { name: /release lfm3-1\.7b/i }));

    expect(onRelease).toHaveBeenCalledWith('text');
    expect(screen.getByRole('button', { name: /release lfm3-vl-2b/i })).toBeDisabled();
  });

  it('explains an unavailable runtime without hiding it', () => {
    render(
      <ModelDock
        models={[]}
        llama={{ ...HEALTH.llama, executable: null, pinned: false }}
        onRelease={vi.fn()}
      />,
    );

    expect(screen.getByText(/runtime not verified/i)).toBeInTheDocument();
    expect(screen.getByText(/no models reported/i)).toBeInTheDocument();
  });
});
