import { mkdirSync } from 'node:fs';

import { expect, test, type Page } from '@playwright/test';

import { HEALTH, MISSION } from '../src/bridge/fixtures';
import type { JobEvent, JobRecord } from '../src/bridge/types';
import { installMockJobEngine, type MockEngineOptions, type MockScenario } from './mockEngine';

const OUT = 'e2e/artifacts';

const INTERRUPTED_JOB: JobRecord = {
  id: 'job-interrupted',
  goal: 'Rewrite the stale notes into out/handover.md',
  deliverable: 'out/handover.md',
  status: 'interrupted',
  stopped: 'the engine stopped before this job finished',
  model: 'lfm3-1.7b',
  max_turns: 24,
  minutes: 30,
  deliverable_proof: null,
  created_at: 1,
  updated_at: 2,
  finished_at: null,
};

const INTERRUPTED_EVENTS: JobEvent[] = [
  {
    job_id: INTERRUPTED_JOB.id,
    seq: 1,
    kind: 'job.started',
    payload: { goal: INTERRUPTED_JOB.goal },
    at: 1,
  },
  {
    job_id: INTERRUPTED_JOB.id,
    seq: 2,
    kind: 'job.interrupted',
    payload: { stopped: INTERRUPTED_JOB.stopped },
    at: 2,
  },
];

test.beforeAll(() => {
  mkdirSync(OUT, { recursive: true });
});

async function openTasks(page: Page, scenario: MockScenario) {
  await page.addInitScript(installMockJobEngine, {
    scenario,
    health: HEALTH,
    mission: MISSION,
    interrupted: { job: INTERRUPTED_JOB, events: INTERRUPTED_EVENTS },
  } satisfies MockEngineOptions);
  await page.goto('/');
  await page.getByRole('button', { name: /verified tasks/i }).click();
  await expect(page.getByRole('heading', { name: 'LFM3 Workspace' })).toBeVisible({
    timeout: 15_000,
  });
}

async function startJob(page: Page, goal = 'Rewrite both notes into out/report.md') {
  await page.getByLabel('Job', { exact: true }).fill(goal);
  await page.getByLabel(/required output/i).fill('out/report.md');
  await page.getByRole('button', { name: /^start job$/i }).click();
  const panel = page.getByRole('region', { name: /job timeline/i });
  await expect(panel).toBeVisible();
  return panel;
}

test('drives a job turn by turn and shows only the measured proof', async ({ page }) => {
  await openTasks(page, 'pass');
  const panel = await startJob(page);

  await expect(panel.getByTestId('job-status')).toHaveText('Running');
  await expect(panel.getByRole('button', { name: /stop job/i })).toBeVisible();
  await page.screenshot({ path: `${OUT}/10-job-running.png`, fullPage: true });

  // The todo list is the model's own, and it moves as the turns land.
  const todos = panel.getByRole('list', { name: /job todos/i });
  await expect(todos.getByRole('listitem').filter({ hasText: 'read both notes' })).toContainText(
    'done',
  );
  await expect(panel.getByText('2 of 2 marked done')).toBeVisible();

  // Every turn renders its calls with the real tool output, not model prose.
  const firstTurn = panel.getByRole('listitem', { name: 'Turn 1' });
  await expect(firstTurn).toContainText('list_files');
  await expect(firstTurn).toContainText('notes/a.md');

  await expect(panel.getByText('wrote 31 bytes to out/report.md')).toBeVisible();
  await expect(panel.getByTestId('job-status')).toHaveText('Passed');
  await expect(panel.getByText(/Proven output: out\/report\.md · 512 bytes/)).toBeVisible();
  await expect(panel.getByText(/sha256 aaaaaaaaaaaaaaaa/)).toBeVisible();
  await expect(panel.getByText(/Stopped: the required output verified/)).toBeVisible();
  await expect(panel.getByRole('button', { name: /stop job/i })).toHaveCount(0);
  await page.screenshot({ path: `${OUT}/11-job-passed.png`, fullPage: true });
});

test('stops a running job without ever reporting it as passed', async ({ page }) => {
  await openTasks(page, 'stop');
  const panel = await startJob(page);

  await expect(panel.getByRole('listitem', { name: 'Turn 2' })).toBeVisible();
  await panel.getByRole('button', { name: /stop job/i }).click();

  await expect(panel.getByTestId('job-status')).toHaveText('Cancelled');
  await expect(panel.getByText(/Stopped: the operator stopped the job/)).toBeVisible();
  await expect(panel.getByText(/Proven output/)).toHaveCount(0);
  await expect(panel.getByText('Passed')).toHaveCount(0);
  await expect(panel.getByRole('button', { name: /stop job/i })).toHaveCount(0);
  // The turns it did take stay on screen as evidence.
  await expect(panel.getByRole('listitem', { name: 'Turn 1' })).toBeVisible();
  await page.screenshot({ path: `${OUT}/12-job-cancelled.png`, fullPage: true });
});

test('recovers a turn the live channel skipped instead of dropping it', async ({ page }) => {
  await openTasks(page, 'gap');
  const panel = await startJob(page);

  await expect(panel.getByText(/Recovering journal after a skipped turn/)).toBeVisible();
  await page.screenshot({ path: `${OUT}/13-job-gap.png`, fullPage: true });

  await expect(panel.getByRole('listitem', { name: 'Turn 3' })).toBeVisible();
  await expect(panel.getByRole('listitem', { name: 'Turn 3' })).toContainText(
    'recording the intermediate read without a live frame',
  );
  await expect(panel.getByText(/Recovering journal after a skipped turn/)).toHaveCount(0);
  await expect(panel.getByTestId('job-status')).toHaveText('Passed');
});

test('opens an interrupted job from history without presenting success', async ({ page }) => {
  await openTasks(page, 'pass');
  const panel = await startJob(page);
  await expect(panel.getByTestId('job-status')).toHaveText('Passed');

  const history = panel.getByRole('button', { name: /Rewrite the stale notes/ });
  await history.click();

  await expect(panel.getByText(/Interrupted — not a success/)).toBeVisible();
  await expect(panel.getByTestId('job-status')).toHaveText('Interrupted');
  await expect(panel.getByText(/Proven output/)).toHaveCount(0);
  await expect(panel.getByRole('button', { name: /stop job/i })).toHaveCount(0);
  await page.screenshot({ path: `${OUT}/14-job-interrupted.png`, fullPage: true });
});

test('refuses to start a job with no goal stated', async ({ page }) => {
  await openTasks(page, 'pass');

  const start = page.getByRole('button', { name: /^start job$/i });
  await expect(start).toBeDisabled();

  await page.getByLabel('Job', { exact: true }).fill('Only a goal');
  await expect(start).toBeEnabled();
  await expect(page.getByLabel(/required output/i)).toHaveValue('');
});
