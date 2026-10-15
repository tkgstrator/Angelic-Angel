import { readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs'
import type { IncomingMessage } from 'node:http'
import { extname, join, resolve } from 'node:path'
import { brotliCompressSync, constants, gzipSync } from 'node:zlib'
import tailwindcss from '@tailwindcss/vite'
import react from '@vitejs/plugin-react'
import type { Plugin, ProxyOptions } from 'vite'
import { defineConfig } from 'vitest/config'

const COMPRESSIBLE = new Set(['.js', '.css', '.html'])

function walk(dir: string): string[] {
  return readdirSync(dir).flatMap((name) => {
    const path = join(dir, name)
    return statSync(path).isDirectory() ? walk(path) : [path]
  })
}

// The Rust server serves precompressed siblings, so emit them at build time.
function precompress(): Plugin {
  let outDir = 'dist'
  return {
    name: 'precompress',
    apply: 'build',
    configResolved(config) {
      outDir = join(config.root, config.build.outDir)
    },
    writeBundle() {
      for (const file of walk(outDir)) {
        if (!COMPRESSIBLE.has(extname(file))) continue
        const data = readFileSync(file)
        writeFileSync(`${file}.gz`, gzipSync(data, { level: 9 }))
        writeFileSync(
          `${file}.br`,
          brotliCompressSync(data, {
            params: {
              [constants.BROTLI_PARAM_QUALITY]: 11,
              [constants.BROTLI_PARAM_SIZE_HINT]: data.length,
            },
          }),
        )
      }
    },
  }
}

const target = process.env.VITE_API_TARGET ?? 'http://127.0.0.1:8080'

const NOTIFICATIONS_PATH = /^\/api\/accounts\/[^/]+\/notifications$/

// 通知 GET の応答は上流の content-length と connection を引き継がず、下流の接続管理を Node に任せる。
export function normalizeNotificationsResponseHeaders(
  proxyRes: Pick<IncomingMessage, 'headers'>,
  req: Pick<IncomingMessage, 'method' | 'url'>,
): void {
  if (req.method === 'GET' && NOTIFICATIONS_PATH.test(req.url?.split('?', 1)[0] ?? '')) {
    delete proxyRes.headers['content-length']
    delete proxyRes.headers.connection
  }
}

// Keep the browser's Host and Origin untouched: the API rejects mutations whose Origin does not
// match Host, so `changeOrigin` must stay false (Vite's string shorthand would turn it on).
const proxy: ProxyOptions = {
  target,
  changeOrigin: false,
  configure: (server) => {
    server.on('proxyRes', normalizeNotificationsResponseHeaders)
  },
}

export default defineConfig({
  plugins: [react(), tailwindcss(), precompress()],
  resolve: { alias: { '@': resolve(import.meta.dirname, 'src') } },
  server: { port: 11475, strictPort: true, proxy: { '/api': proxy, '/health': proxy } },
  build: { outDir: 'dist', assetsDir: 'assets', sourcemap: false, target: 'es2022' },
  test: {
    environment: 'happy-dom',
    setupFiles: ['./src/test/setup.ts'],
    css: false,
  },
})
