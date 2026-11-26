import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// 产物输出到 frontend/dist，再由构建脚本复制进 internal/uits/page，
// 用 go:embed 打进二进制（见 internal/uits/statics.go）。
export default defineConfig({
  plugins: [vue()],
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // 面板只在现代浏览器里跑，不需要为旧浏览器降级语法。
    target: "chrome120",
  },
  server: {
    port: 9245,
    strictPort: true,
  },
});
