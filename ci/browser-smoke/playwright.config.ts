import { defineConfig } from '@playwright/test';

// The smoke server (examples/openapi-ui) serves the documentation UI on fixed
// loopback ports: unauthenticated on the application listener (openapi.public)
// and behind this bearer token on the management listener.
const token = process.env.FERRUM_ALLOY_MANAGEMENT_TOKEN;
if (!token) {
  throw new Error('Set FERRUM_ALLOY_MANAGEMENT_TOKEN to the management token of the smoke server');
}

export default defineConfig({
  testDir: 'tests',
  outputDir: 'test-results',
  forbidOnly: true,
  retries: 0,
  // One browser at a time keeps the management listener's rate limit out of play.
  workers: 1,
  timeout: 60_000,
  expect: { timeout: 15_000 },
  reporter: [['list'], ['html', { open: 'never', outputFolder: 'playwright-report' }]],
  use: {
    // The runner's preinstalled Google Chrome, so no browser is downloaded.
    channel: 'chrome',
    headless: true,
    colorScheme: 'light',
    // The page's Content-Security-Policy is what is under test.
    bypassCSP: false,
    screenshot: 'only-on-failure',
    trace: 'retain-on-failure',
  },
  projects: [
    {
      name: 'public',
      use: { baseURL: 'http://127.0.0.1:18080' },
    },
    {
      name: 'management',
      use: {
        baseURL: 'http://127.0.0.1:19090',
        extraHTTPHeaders: { authorization: `Bearer ${token}` },
      },
    },
  ],
});
