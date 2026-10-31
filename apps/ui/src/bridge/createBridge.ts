import type { EngineBridge } from './EngineBridge';
import { TauriBridge } from './TauriBridge';

/**
 * Demo fixtures are a development-only aid. The branch is compiled out of the
 * production bundle, so a packaged app can only select the Tauri bridge.
 */
export async function createBridge(): Promise<EngineBridge> {
  if (import.meta.env.DEV && new URLSearchParams(window.location.search).get('demo') === '1') {
    const { DemoBridge } = await import('./DemoBridge');
    const scenario = new URLSearchParams(window.location.search).get('scenario');
    return new DemoBridge(scenario);
  }
  return new TauriBridge();
}
