import { defineConfig, devices } from '@playwright/test';

// 真机验收：连到已经在跑的控制台（e2e/real/run-windows.ps1 起的，或手工起的），不自带服务。
// CI 不跑——没有被测设备。用例文件名是 *.real.ts，默认的 playwright.config.ts 匹配不到。
export default defineConfig({
  testDir: './e2e/real',
  testMatch: '**/*.real.ts',
  forbidOnly: true,
  workers: 1,
  retries: 0,
  timeout: 30 * 60_000,
  outputDir: 'test-results/real',
  // 整轮可以跑很久（上面的 timeout），但单个界面动作卡住要尽快失败，而不是等满半小时。
  expect: { timeout: 30_000 },
  use: { baseURL: process.env.CPE_REAL_URL ?? 'http://127.0.0.1:29889', trace: 'retain-on-failure', actionTimeout: 30_000 },
  projects: [{ name: 'chromium', use: { ...devices['Desktop Chrome'] } }],
});
