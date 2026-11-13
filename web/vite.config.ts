import { fileURLToPath, URL } from 'node:url'
import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'

// `bun dev --host` binds 0.0.0.0 so LAN clients (and the Windows browser when
// this runs in WSL2) can reach the dev server. Vite validates the Host header:
// bare IPs are always fine, but a hostname like `dev-host.local` is rejected
// unless it is listed here. IYUU_ALLOWED_HOSTS takes a comma-separated list;
// a leading dot matches the domain and all of its subdomains.
const allowedHosts = (process.env.IYUU_ALLOWED_HOSTS ?? '')
  .split(',')
  .map((h) => h.trim())
  .filter(Boolean)

export default defineConfig({
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: { '@': fileURLToPath(new URL('./src', import.meta.url)) },
  },
  server: {
    port: 5173,
    // Bind all interfaces by default so the dev server is reachable from the
    // LAN without having to pass --host on every invocation.
    host: true,
    ...(allowedHosts.length > 0 ? { allowedHosts } : {}),
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
