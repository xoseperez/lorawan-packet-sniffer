import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import pkg from './package.json';

export default defineConfig({
  plugins: [react()],
  // Shown in the footer. Read from package.json rather than the Rust crate
  // because the Docker WebUI stage only has the webui/ folder.
  define: {
    __APP_VERSION__: JSON.stringify(pkg.version),
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
  server: {
    // Proxy API and WS to the Rust backend when running `npm run dev`
    proxy: {
      '/ws': { target: 'ws://localhost:8888', ws: true },
      '/api': { target: 'http://localhost:8888' },
    },
  },
});
