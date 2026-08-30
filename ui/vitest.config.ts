import { defineConfig } from 'vitest/config';

// `environment: 'node'` 是有意的，不是省事：这里**不装** jsdom / @vue/test-utils。
//
// 回顾旧 `src/master/webui/tests.rs` 里那四个页面测试记录下来的真实缺口——角色键
// 忽略 `pair.cross`、整列开关缺分支、`-l` 被全局档位反向覆写、配置卡片没有删除
// 入口——**没有一个是渲染问题**，全是纯逻辑。新架构把这些赶进 `src/domain/`
// （由 `lint-arch.mjs` 挡着不许 import vue），普通 Vitest 就能覆盖，而且比挂载
// 组件断言 DOM 结实得多。
//
// 组件测试等到出现第一个**真的只在渲染层出现**的 bug 再加。在那之前，jsdom 只是
// 让 `npm ci` 更慢、依赖面更大。详见 .ai/PLAN-v5.0-frontend.md §7.2。
export default defineConfig({
  test: {
    environment: 'node',
    include: ['src/**/*.{test,spec}.ts'],
    // 模块级 reactive 是单例，用例之间必须互不串味。每个 state 模块导出的
    // reset() 是给这件事用的（§4.2），这里再加一道隔离。
    restoreMocks: true,
  },
});
