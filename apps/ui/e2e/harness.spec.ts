import { mkdirSync } from 'node:fs';

import { expect, test, type Page } from '@playwright/test';

const HASH = 'f7ed5f17bda3c957797569028aab3e375b42998cbf65d489bdc7a9e8e7b6aba5';
const OUT = 'e2e/artifacts';

test.beforeAll(() => {
  mkdirSync(OUT, { recursive: true });
});

async function openScenario(page: Page, scenario = 'workflow') {
  await page.goto(`/?demo=1&scenario=${scenario}`);
  await expect(page.getByRole('heading', { name: 'LFM3 Workspace' })).toBeVisible();
  await expect(page.getByText('Demo data')).toBeVisible();
}

async function startAndApprove(page: Page) {
  await page.getByLabel('Goal').fill('Write the handover summary');
  await page.getByRole('button', { name: /create demo run/i }).click();
  await expect(page.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();
  await expect(page.getByTestId('acceptance-hash')).toHaveText(HASH);
  await page.getByRole('button', { name: /approve plan/i }).click();
}

test('starts a mission and reviews the frozen acceptance', async ({ page }) => {
  await openScenario(page);
  await page.screenshot({ path: `${OUT}/01-mission.png`, fullPage: true });

  await page.getByLabel('Goal').fill('Write the handover summary');
  await page.getByRole('button', { name: /create demo run/i }).click();

  const acceptance = page.getByRole('region', { name: /frozen acceptance/i });
  await expect(acceptance.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();
  await expect(page.getByTestId('acceptance-hash')).toHaveText(HASH);
  await expect(acceptance.getByText(/out\/summary\.md exists and contains/i)).toBeVisible();
  await page.screenshot({ path: `${OUT}/02-acceptance.png`, fullPage: true });
});

test('observes a failed attempt, approves the command, and reaches final evidence', async ({
  page,
}) => {
  await openScenario(page, 'workflow');
  await startAndApprove(page);

  const timeline = page.getByRole('region', { name: /run timeline/i });
  await expect(timeline.getByText(/does not contain "owner"/)).toBeVisible();
  await expect(timeline.getByText(/strategy 2/i)).toBeVisible();

  const dialog = page.getByRole('dialog', { name: /approval requested/i });
  await expect(dialog).toBeVisible();
  await page.screenshot({ path: `${OUT}/03-active-retry.png`, fullPage: true });
  await dialog.getByRole('button', { name: /allow once/i }).click();

  const evidence = page.getByRole('region', { name: /final evidence/i });
  await expect(evidence.getByRole('heading', { name: 'Final evidence' })).toBeVisible();
  await expect(evidence.getByText('out/summary.md matches every required condition')).toBeVisible();
  await page.screenshot({ path: `${OUT}/04-evidence.png`, fullPage: true });
});

test('denies a command and keeps the run incomplete', async ({ page }) => {
  await openScenario(page, 'deny');
  await startAndApprove(page);

  const dialog = page.getByRole('dialog', { name: /approval requested/i });
  await expect(dialog).toBeVisible();
  await dialog.getByRole('button', { name: /deny/i }).click();

  const timeline = page.getByRole('region', { name: /run timeline/i });
  await expect(timeline.getByText('Incomplete')).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Final evidence' })).toHaveCount(0);
  await page.screenshot({ path: `${OUT}/05-incomplete.png`, fullPage: true });
});

test('cancels during execution and disables further pass transitions', async ({ page }) => {
  await openScenario(page, 'cancel');
  await startAndApprove(page);

  const timeline = page.getByRole('region', { name: /run timeline/i });
  await expect(timeline.getByText(/strategy 2/i)).toBeVisible();
  await timeline.getByRole('button', { name: /cancel run/i }).click();

  await expect(timeline.getByText('Cancelled', { exact: true })).toBeVisible();
  await expect(timeline.getByRole('button', { name: /cancel run/i })).toBeDisabled();
  await expect(page.getByRole('heading', { name: 'Final evidence' })).toHaveCount(0);
});

test('recovers through a journal gap before showing the final state', async ({ page }) => {
  await openScenario(page, 'gap');
  await startAndApprove(page);

  await expect(page.getByRole('status')).toContainText(/recovering journal/i);
  await page.screenshot({ path: `${OUT}/07-gap-recovery.png`, fullPage: true });
  await page.evaluate(() => {
    (
      globalThis as unknown as { __lfm3Demo: { releaseGap(): void } }
    ).__lfm3Demo.releaseGap();
  });

  await expect(page.getByRole('heading', { name: 'Final evidence' })).toBeVisible();
});

test('rejects a second active run instead of starting it', async ({ page }) => {
  await openScenario(page, 'workflow');
  await page.getByLabel('Goal').fill('Write the handover summary');
  await page.getByRole('button', { name: /create demo run/i }).click();
  await expect(page.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();

  await page.getByRole('button', { name: /create demo run/i }).click();

  await expect(page.getByRole('alert')).toContainText(/another run is active/i);
  await expect(page.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();
});

test('retries an incomplete run and opens the linked child from history', async ({ page }) => {
  await openScenario(page, 'deny');
  await startAndApprove(page);
  await page
    .getByRole('dialog', { name: /approval requested/i })
    .getByRole('button', { name: /deny/i })
    .click();
  const timeline = page.getByRole('region', { name: /run timeline/i });
  await expect(timeline.getByText('Incomplete')).toBeVisible();

  await timeline.getByRole('button', { name: /retry run/i }).click();

  await expect(page.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();
  const linked = page.getByRole('row', { name: /retry of/i });
  await expect(linked).toBeVisible();
  await page.screenshot({ path: `${OUT}/08-linked-retry.png`, fullPage: true });
  await linked.getByRole('button', { name: /open run/i }).click();
  await expect(page.getByRole('heading', { name: /frozen acceptance/i })).toBeVisible();
});

test('opens an interrupted run as history without presenting success', async ({ page }) => {
  await openScenario(page);

  const interrupted = page.getByRole('row', { name: /interrupted/i });
  await interrupted.getByRole('button', { name: /open run/i }).click();

  await expect(page.getByText(/Interrupted — not a success/i)).toBeVisible();
  await expect(page.getByRole('heading', { name: 'Final evidence' })).toHaveCount(0);
  await page.screenshot({ path: `${OUT}/06-interrupted.png`, fullPage: true });
});
