import { defineConfig, devices } from '@playwright/test';

// 使用真实 Rust 传输层与已构建的内联页面；禁止复用未知版本的本机服务。
const port = process.env.CPE_BROWSER_TEST_PORT ?? '29876';
const baseURL = `http://127.0.0.1:${port}`;
export default defineConfig({
  testDir: './e2e',
  testMatch: '**/*.spec.ts',
  forbidOnly: !!process.env.CI,
  workers: 1,
  retries: 0,
  timeout: 30_000,
  use: { baseURL, trace: 'retain-on-failure' },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
  webServer: {
    command: 'cargo test --locked browser_regression_server -- --ignored --nocapture',
    cwd: '..',
    env: { CPE_BROWSER_TEST_PORT: port },
    url: `${baseURL}/?token=browser-regression-secret`,
    reuseExistingServer: false,
    timeout: 180_000,
  },
});
