import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
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
