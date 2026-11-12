import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// In dev, the Rust server runs on :8080 and Vite proxies API calls to it.
export default defineConfig({
  plugins: [react()],
  server: { proxy: { '/api': 'http://127.0.0.1:8080' } },
})
