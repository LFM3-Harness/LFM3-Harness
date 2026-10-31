import { render, screen, within } from '@testing-library/react';
import userEvent from '@testing-library/user-event';
import type { ComponentProps } from 'react';
import { describe, expect, it, vi } from 'vitest';

import type { ModelCatalog } from '../bridge/types';
import { ChatModelMenu } from './ChatModelMenu';

function catalog(overrides: Partial<ModelCatalog> = {}): ModelCatalog {
  return {
    models_dir: 'F:\\lfm3\\models',
    download: null,
    pending_reload: false,
    runtime_ready: true,
    runtime_note: null,
    models: [
      {
        key: 'text',
        title: 'Text and agent',
        repo: 'LFM3-1.7B-GGUF',
        source: 'LiquidAI/LFM3-1.7B-GGUF',
        license: 'LFM Open License (see the model card)',
        total_bytes: 1_099_627_808,
        files: [{ name: 'LFM3-1.7B-Q4_K_M.gguf', bytes: 1_099_627_808, sha256: 'a'.repeat(64) }],
        installed: true,
        loaded: true,
        state: 'loaded',
      },
      {
        key: 'vision',
        title: 'Vision',
        repo: 'LFM3-VL-2B-GGUF',
        source: 'LiquidAI/LFM3-VL-2B-GGUF',
        license: 'LFM Open License (see the model card)',
        total_bytes: 1_252_647_520,
        files: [],
        installed: true,
        loaded: false,
        state: 'installed',
      },
      {
        key: 'audio',
        title: 'Voice in and out',
        repo: 'LFM3-Audio-2B-GGUF',
        source: 'LiquidAI/LFM3-Audio-2B-GGUF',
        license: 'LFM Open License (see the model card)',
        total_bytes: 1_403_437_856,
        files: [],
        installed: false,
        loaded: false,
        state: 'absent',
      },
      {
        key: 'd1',
        title: 'Decision',
        repo: 'd1-2B-GGUF',
        source: 'LiquidAI/d1-2B-GGUF',
        license: 'LFM Open License (see the model card)',
        total_bytes: 1_099_629_888,
        files: [],
        installed: false,
        loaded: false,
        state: 'absent',
      },
    ],
    ...overrides,
  };
}

function menu(overrides: Partial<ComponentProps<typeof ChatModelMenu>> = {}) {
  const props: ComponentProps<typeof ChatModelMenu> = {
    catalog: catalog(),
    downloading: null,
    progress: null,
    error: null,
    refreshing: false,
    busy: false,
    busyReason: null,
    onRefresh: vi.fn(),
    onDownload: vi.fn(),
    onCancel: vi.fn(),
    onReload: vi.fn(),
    onClose: vi.fn(),
    ...overrides,
  };
  render(<ChatModelMenu {...props} />);
  return props;
}

describe('ChatModelMenu', () => {
  it('reports loaded, installed, and missing roles from the engine catalog', () => {
    menu();

    expect(screen.getByText('Loaded')).toBeInTheDocument();
    expect(screen.getByText('Installed')).toBeInTheDocument();
    expect(screen.getAllByText('Missing')).toHaveLength(2);
    expect(screen.getByText(/F:\\lfm3\\models/)).toBeInTheDocument();
    expect(screen.getByText(/from LiquidAI on Hugging Face/)).toBeInTheDocument();
    expect(screen.getByText(/SHA-256/)).toBeInTheDocument();
  });

  it('only offers Download for a missing role and reports the click', async () => {
    const user = userEvent.setup();
    const props = menu();

    const decision = screen.getByText('Decision').closest('li');
    expect(decision).not.toBeNull();
    await user.click(within(decision as HTMLElement).getByRole('button', { name: /download/i }));

    expect(props.onDownload).toHaveBeenCalledWith('d1');
    expect(within(decision as HTMLElement).getAllByText(/1\.0 GB/).length).toBeGreaterThan(0);
  });

  it('shows live progress and cancels the active role', async () => {
    const user = userEvent.setup();
    const props = menu({
      downloading: 'audio',
      progress: {
        key: 'audio',
        file: 'LFM3-Audio-2B-Q4_K_M.gguf',
        received: 512,
        fileTotal: 1024,
        overallReceived: 50,
        overallTotal: 100,
      },
    });

    expect(screen.getByRole('progressbar')).toHaveAttribute('aria-valuenow', '50');
    expect(screen.getByText(/50% · 50 B of 100 B/)).toBeInTheDocument();

    await user.click(screen.getByRole('button', { name: /^cancel$/i }));
    expect(props.onCancel).toHaveBeenCalledWith('audio');
  });

  it('does not start a download while a turn is running', () => {
    menu({ busy: true, busyReason: 'Stop the current reply.' });

    for (const button of screen.getAllByRole('button', { name: /download/i })) {
      expect(button).toBeDisabled();
      expect(button).toHaveAttribute('title', 'Stop the current reply.');
    }
  });

  it('offers a reload for a finished download that is waiting', async () => {
    const user = userEvent.setup();
    const props = menu({ catalog: catalog({ pending_reload: true }) });

    expect(screen.getByText(/waiting for this conversation to go idle/i)).toBeInTheDocument();
    await user.click(screen.getByRole('button', { name: /load now/i }));

    expect(props.onReload).toHaveBeenCalled();
  });

  it('warns when the pinned runtime is missing without hiding the catalog', () => {
    menu({
      catalog: catalog({ runtime_ready: false, runtime_note: 'no llama-server binary found' }),
    });

    expect(screen.getByText(/no llama-server binary found/)).toBeInTheDocument();
    expect(screen.getByText('Text and agent')).toBeInTheDocument();
  });
});
