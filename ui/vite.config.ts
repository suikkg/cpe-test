import { defineConfig } from 'vite';
import vue from '@vitejs/plugin-vue';
import { viteSingleFile } from 'vite-plugin-singlefile';

// 产物必须是**单个全内联的 HTML**。这不是审美选择：控制台的鉴权在路由之前
// （master/webui/http.rs::handle），浏览器不会给 <script src> 带自定义头，
// 所以任何外链子资源都会被挡成 401。详见 AGENTS.md §4 与 .ai/DESIGN-v5.0-webui.md §3。
export default defineConfig({
  plugins: [vue(), viteSingleFile({ removeViteModuleLoader: true })],
  build: {
    // 目标机器是 Windows 上的现代 Chrome/Edge，不需要更低的基线。
    target: 'es2020',
    cssCodeSplit: false,
    // 内联一切，别让 Vite 因为体积把资源甩成外链文件。
    assetsInlineLimit: 100 * 1024 * 1024,
    chunkSizeWarningLimit: 100 * 1024 * 1024,
    reportCompressedSize: false,
    emptyOutDir: true,
    // source map 会被内联进产物，体积暴涨且毫无用处（用户拿到的是 exe）。
    sourcemap: false,
  },
});
