import { defineConfig } from "vite";
import { resolve } from "path";

export default defineConfig({
  clearScreen: false,
  server: {
    host: "127.0.0.1",
    port: 5173,
    strictPort: true,
    // 纯浏览器开发时：ZSW_SERVER_PORT=17823 npm run dev，/api 代理到 zsw-server
    proxy: process.env.ZSW_SERVER_PORT
      ? { "/api": { target: `http://127.0.0.1:${process.env.ZSW_SERVER_PORT}` } }
      : undefined,
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        settings: resolve(__dirname, "settings.html"),
        captcha: resolve(__dirname, "captcha.html"),
      },
    },
  },
});
