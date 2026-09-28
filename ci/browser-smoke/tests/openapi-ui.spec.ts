// Loads Ferrum Alloy's OpenAPI documentation UI (feature `openapi-ui`) in
// Google Chrome and proves that Swagger UI renders under the page's
// Content-Security-Policy with nothing blocked, and that the browser really
// enforces that policy (the negative control). The server is
// examples/openapi-ui; its document is checked by that crate's tests.

import { readFileSync } from 'node:fs';
import { join } from 'node:path';
import { expect, test, type Page, type Response } from '@playwright/test';

const UI_PATH = '/docs';
const DOCUMENT_PATH = '/openapi.json';
const ASSETS = ['swagger-ui.css', 'swagger-ui-bundle.js', 'swagger-initializer.js'];
const TITLE = 'Ferrum Alloy browser smoke';
const OPERATIONS = 2;

const REPOSITORY = join(__dirname, '..', '..', '..');
const SCREENSHOTS = join(__dirname, '..', 'screenshots');

type Violation = {
  directive: string;
  blockedURI: string;
  sourceFile: string;
  disposition: string;
};

/**
 * The policy Alloy sends with the page and its assets, read from the Rust
 * source that defines it, so the browser test and the server cannot drift.
 */
function expectedPolicy(): string {
  const source = readFileSync(
    join(REPOSITORY, 'crates', 'ferrum-alloy', 'src', 'openapi_ui.rs'),
    'utf8',
  );
  const literal = /const CONTENT_SECURITY_POLICY_VALUE: &str = "((?:[^"\\]|\\.)*)";/s.exec(source);
  if (!literal) {
    throw new Error('CONTENT_SECURITY_POLICY_VALUE not found in openapi_ui.rs');
  }
  // A backslash at the end of a line continues a Rust string literal and
  // skips the next line's leading whitespace. No other escape is expected.
  const policy = literal[1].replace(/\\\n\s*/g, '');
  if (policy.includes('\\') || !policy.includes("script-src 'self'")) {
    throw new Error(`unexpected policy literal: ${policy}`);
  }
  return policy;
}

type Watched = {
  consoleErrors: string[];
  requests: string[];
  failedRequests: string[];
  violations: () => Promise<Violation[]>;
};

/** What the current test's page recorded, reported after the test. */
let current: Watched | undefined;

/**
 * Records, from before the first navigation, every CSP violation the page
 * reports, every console error and uncaught exception, and every request.
 */
async function watch(page: Page): Promise<Watched> {
  const consoleErrors: string[] = [];
  const requests: string[] = [];
  const failedRequests: string[] = [];
  page.on('console', (message) => {
    if (message.type() === 'error') {
      consoleErrors.push(message.text());
    }
  });
  page.on('pageerror', (error) => consoleErrors.push(`uncaught: ${error.message}`));
  page.context().on('request', (request) => requests.push(request.url()));
  page.context().on('requestfailed', (request) => {
    failedRequests.push(`${request.url()}: ${request.failure()?.errorText}`);
  });
  await page.addInitScript(() => {
    const violations: unknown[] = [];
    Object.defineProperty(window, '__alloyCspViolations', { value: violations });
    document.addEventListener(
      'securitypolicyviolation',
      (event) => {
        violations.push({
          directive: event.effectiveDirective,
          blockedURI: event.blockedURI,
          sourceFile: event.sourceFile,
          disposition: event.disposition,
        });
      },
      true,
    );
  });
  const violations = () =>
    page.evaluate(
      () => (window as unknown as { __alloyCspViolations: Violation[] }).__alloyCspViolations,
    );
  current = { consoleErrors, requests, failedRequests, violations };
  return current;
}

/** Whether `url` leaves `origin`. `data:` URIs are inline content, not requests. */
function isForeign(url: string, origin: string): boolean {
  const parsed = new URL(url);
  return parsed.protocol !== 'data:' && parsed.origin !== origin;
}

// Attach what the browser recorded to every test, and print it when a test
// fails, so a blocked resource is reported with the browser's own words even
// when an earlier assertion (such as the page never rendering) failed first.
test.afterEach(async ({}, testInfo) => {
  const watched = current;
  current = undefined;
  if (!watched) {
    return;
  }
  const violations = await watched
    .violations()
    .catch((error: Error) => `unavailable: ${error.message}`);
  const body = JSON.stringify(
    {
      violations,
      consoleErrors: watched.consoleErrors,
      failedRequests: watched.failedRequests,
      requests: watched.requests,
    },
    null,
    2,
  );
  await testInfo.attach('browser-evidence', { body, contentType: 'application/json' });
  if (testInfo.status !== testInfo.expectedStatus) {
    console.log(`Browser evidence (${testInfo.project.name}, ${testInfo.title}):\n${body}`);
  }
});

test('the documentation UI renders under its Content-Security-Policy', async ({
  page,
  baseURL,
}, testInfo) => {
  const origin = new URL(baseURL ?? '').origin;
  const watched = await watch(page);
  const responses = new Map<string, Response>();
  page.on('response', (response) => {
    responses.set(new URL(response.url()).pathname, response);
  });

  const navigation = await page.goto(UI_PATH);
  expect(navigation?.status()).toBe(200);

  // Swagger UI fetched the document and rendered it.
  const title = page.locator('.swagger-ui .info .title');
  await expect(title).toContainText(TITLE);
  const operations = page.locator('.swagger-ui .opblock');
  await expect(operations).toHaveCount(OPERATIONS);
  await expect(operations.first()).toBeVisible();
  await expect(page.locator('.swagger-ui .opblock-summary-path').first()).toContainText('/orders');
  await expect(page.locator('.swagger-ui section.models')).toBeVisible();
  // The stylesheet applied: Swagger UI's sizes and colors, not the browser's.
  await expect(title).toHaveCSS('font-size', '36px');
  await expect(page.locator('.swagger-ui .opblock.opblock-get').first()).toHaveCSS(
    'border-top-color',
    'rgb(97, 175, 254)',
  );
  // Expanding an operation renders its parameters, responses, and examples.
  const first = operations.first();
  await first.locator('.opblock-summary-control').click();
  await expect(first.locator('.opblock-body')).toBeVisible();
  await expect(first.locator('.responses-wrapper')).toContainText('200');

  // Everything the rendered page loads has settled before judging.
  await page.waitForLoadState('networkidle');
  await page.screenshot({
    path: join(SCREENSHOTS, `openapi-ui-${testInfo.project.name}.png`),
    fullPage: true,
  });

  // The page and every asset it loaded carry exactly Alloy's policy.
  const policy = expectedPolicy();
  for (const path of [UI_PATH, ...ASSETS.map((asset) => `${UI_PATH}/${asset}`)]) {
    const response = responses.get(path);
    expect(response, `${path} was loaded`).toBeDefined();
    expect(response?.status(), path).toBe(200);
    expect(await response?.headerValues('content-security-policy'), path).toEqual([policy]);
  }
  expect(responses.get(DOCUMENT_PATH)?.status(), 'the document was loaded').toBe(200);

  expect(
    watched.requests.filter((url) => isForeign(url, origin)),
    'requests to another origin',
  ).toEqual([]);
  expect(await watched.violations(), 'CSP violations').toEqual([]);
  expect(watched.consoleErrors, 'console errors').toEqual([]);
  expect(watched.failedRequests, 'failed requests').toEqual([]);
});

test('negative control: the browser blocks and reports an injected inline script', async ({
  page,
}) => {
  const watched = await watch(page);
  await page.goto(UI_PATH);
  await expect(page.locator('.swagger-ui .info .title')).toContainText(TITLE);
  expect(await watched.violations(), 'no violation before the injection').toEqual([]);

  // Depending on timing, Playwright either resolves or rejects with the
  // browser's CSP console message; the script must not run either way.
  const rejection = await page
    .addScriptTag({ content: 'window.__alloyInlineScriptRan = true;' })
    .then(() => null, (error: Error) => error);
  if (rejection) {
    expect(rejection.message).toContain('Content Security Policy');
  }

  await expect.poll(watched.violations).toHaveLength(1);
  const [violation] = await watched.violations();
  expect(violation.directive).toMatch(/^script-src(-elem)?$/);
  expect(violation.blockedURI).toBe('inline');
  expect(violation.disposition).toBe('enforce');
  const ran = await page.evaluate(() => '__alloyInlineScriptRan' in window);
  expect(ran, 'the inline script ran').toBe(false);
  await expect.poll(() => watched.consoleErrors).toHaveLength(1);
  expect(watched.consoleErrors[0]).toContain('Content Security Policy');
});

test('the management listener serves the UI only with the token', async ({
  playwright,
  baseURL,
}, testInfo) => {
  test.skip(testInfo.project.name !== 'management', 'the public listener has no token');
  // A context created inside a test inherits the project's `use` options,
  // including the token in `extraHTTPHeaders`, so each one here sets its
  // headers explicitly.
  for (const [label, headers] of [
    ['no token', {}],
    ['a wrong token', { authorization: 'Bearer not-the-management-token' }],
  ] as const) {
    const context = await playwright.request.newContext({ baseURL, extraHTTPHeaders: headers });
    try {
      for (const path of [UI_PATH, `${UI_PATH}/swagger-ui-bundle.js`, DOCUMENT_PATH]) {
        const response = await context.get(path);
        expect(response.status(), `${path} with ${label}`).toBe(401);
      }
    } finally {
      await context.dispose();
    }
  }
  // And the same request with the token is served, so the 401s above come
  // from the token check, not from a missing route.
  const token = process.env.FERRUM_ALLOY_MANAGEMENT_TOKEN;
  const authorized = await playwright.request.newContext({
    baseURL,
    extraHTTPHeaders: { authorization: `Bearer ${token}` },
  });
  try {
    const response = await authorized.get(UI_PATH);
    expect(response.status(), `${UI_PATH} with the token`).toBe(200);
  } finally {
    await authorized.dispose();
  }
});
