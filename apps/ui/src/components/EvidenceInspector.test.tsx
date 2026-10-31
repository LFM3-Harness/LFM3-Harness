import { render, screen } from '@testing-library/react';
import { describe, expect, it } from 'vitest';

import { EvidenceInspector } from './EvidenceInspector';
import {
  PASSED_EVENTS,
  PASSED_SNAPSHOT,
  SEMANTIC_ENGINE_EVENTS,
  SEMANTIC_EVENTS,
} from '../testFixtures';

describe('EvidenceInspector', () => {
  it('shows final check evidence and artifact digests for a passed run', () => {
    render(<EvidenceInspector snapshot={PASSED_SNAPSHOT} events={PASSED_EVENTS} />);

    expect(screen.getByText(/final evidence/i)).toBeInTheDocument();
    expect(screen.getByText('content_matches')).toBeInTheDocument();
    expect(screen.getByText(/matches every required condition/i)).toBeInTheDocument();
    expect(screen.getByText('workspace:out/summary.md')).toBeInTheDocument();
    expect(screen.getByText(/3b7c1d9e5f0a4c2b/)).toBeInTheDocument();
  });

  it('labels semantic verdicts as advisory with their confidence', () => {
    render(<EvidenceInspector snapshot={PASSED_SNAPSHOT} events={SEMANTIC_EVENTS} />);

    expect(screen.getByText(/semantic confidence 82% \(advisory\)/i)).toBeInTheDocument();
    expect(screen.getByText(/semantic confidence 44% \(advisory, low\)/i)).toBeInTheDocument();
    expect(screen.getAllByText(/semantic_support/)).toHaveLength(2);
  });

  it('labels engine semantic events with basis points, chain, and label', () => {
    render(<EvidenceInspector snapshot={PASSED_SNAPSHOT} events={SEMANTIC_ENGINE_EVENTS} />);

    expect(screen.getByText('d1_grounded')).toBeInTheDocument();
    expect(
      screen.getByText(/semantic confidence 93% \(advisory\) · chain vl->d1 · supported/i),
    ).toBeInTheDocument();
  });

  it('shows a failed check without promoting it', () => {
    render(<EvidenceInspector snapshot={PASSED_SNAPSHOT} events={SEMANTIC_EVENTS} />);

    const rows = screen.getAllByTestId('check-row');
    expect(rows.some((row) => row.dataset.status === 'fail')).toBe(true);
    expect(screen.queryByText(/final evidence/i)).not.toBeInTheDocument();
  });
});
