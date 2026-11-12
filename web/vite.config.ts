import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  server: {
    port: 5173,
    // During `bun dev` the API lives on the Rust server; proxy so the SPA can
    // use same-origin relative URLs in both dev and production.
    proxy: {
      '/api': {
        target: process.env.IYUU_API ?? 'http://127.0.0.1:8098',
        changeOrigin: true,
      },
    },
  },
  build: { outDir: 'dist', emptyOutDir: true },
})
