import { defineConfig } from 'vite';

// https://vitejs.dev/config/
export default defineConfig({
  // Prevent vite from obscuring rust errors in the dev console.
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    watch: {
      // Tell vite to ignore watching `src-tauri` (Rust changes rebuild via cargo).
      ignored: ['**/src-tauri/**'],
    },
  },
  // Tauri uses Chromium on Windows + Linux, WebKit on macOS.
  build: {
    target: ['es2021', 'chrome105', 'safari14'],
    minify: 'esbuild',
    sourcemap: true,
  },
});
