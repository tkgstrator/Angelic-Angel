// Development launcher: `bun dev` from the repository root.
//
// 1. `cargo build --locked`, then run the resulting `angelic-angel serve` (API).
// 2. Run the Vite dev server (WebUI) through bun, proxying /api to the API.
//
// Every child runs in its own process group, so a signal, or the death of any
// child, stops the whole owned tree (descendants such as rustc included). Nothing
// is restarted or watched. POSIX only.
//
// Environment (all optional):
//   DEV_CONFIG    config file for `serve` (default: angelic-angel.toml in the repo root)
//   DEV_API_PORT  API port on 127.0.0.1 (default: 8080)
//   DEV_WEB_PORT  Vite port on 127.0.0.1 (default: 11475)

import { type ChildProcess, spawn, spawnSync } from 'node:child_process'
import { existsSync } from 'node:fs'
import { resolve } from 'node:path'

const ROOT = resolve(import.meta.dirname, '..')
const WEB_DIR = resolve(ROOT, 'web')
const BIN_NAME = 'angelic-angel'
const API_HOST = '127.0.0.1'
// The WebUI is local-only and has no authentication, so it never listens beyond loopback.
const WEB_HOST = '127.0.0.1'

// `serve` waits up to 10 s for connections to drain and then awaits the receiver
// cleanup, so the force deadline has to be comfortably longer than that.
const STOP_GRACE_MS = 20_000
const KILL_WAIT_MS = 3_000
const POLL_MS = 200
const REPEAT_IGNORE_MS = 1_000

type Exit = {
  code: number | null
  signal: NodeJS.Signals | null
  error?: string
}

type Owned = {
  name: string
  child: ChildProcess
  pgid: number | undefined
  exited: Promise<Exit>
}

const owned = new Set<Owned>()
let stopSignal: NodeJS.Signals | null = null
let stopSignalAt = 0
let forced = false
let stopTask: Promise<boolean> | null = null
let requestStop: (signal: NodeJS.Signals) => void = () => {}
const stopRequested = new Promise<NodeJS.Signals>((resolveStop) => {
  requestStop = resolveStop
})

function log(message: string): void {
  console.log(`[dev] ${message}`)
}

function fail(message: string): never {
  console.error(`[dev] ${message}`)
  process.exit(1)
}

function sleep(ms: number): Promise<void> {
  return new Promise((done) => setTimeout(done, ms))
}

function readPort(name: string, fallback: number): number {
  const raw = process.env[name]
  if (raw === undefined || raw === '') return fallback
  const value = Number(raw)
  if (!Number.isInteger(value) || value < 1 || value > 65535) {
    fail(`${name} must be an integer between 1 and 65535`)
  }
  return value
}

// ---- process groups ----

function signalGroup(pgid: number, signal: NodeJS.Signals): void {
  try {
    process.kill(-pgid, signal)
  } catch (error) {
    // The group is already gone.
    if (!(error instanceof Error && 'code' in error && error.code === 'ESRCH')) throw error
  }
}

// Zombies still count for kill(-pgid, 0) when PID 1 does not reap (as in dev
// containers), so count only members that are not defunct.
function liveMembers(pgid: number): number {
  const ps = spawnSync('ps', ['-A', '-o', 'pgid=', '-o', 'stat='], {
    encoding: 'utf8',
  })
  if (ps.status !== 0) {
    try {
      process.kill(-pgid, 0)
      return 1
    } catch {
      return 0
    }
  }
  let count = 0
  for (const line of ps.stdout.split('\n')) {
    const [group, stat] = line.trim().split(/\s+/)
    if (Number(group) === pgid && stat !== undefined && !stat.startsWith('Z')) count++
  }
  return count
}

/** Signals the whole group and waits for it to empty, escalating to SIGKILL. */
async function stopGroup(item: Owned, signal: NodeJS.Signals): Promise<boolean> {
  const { pgid } = item
  if (pgid === undefined) return true
  signalGroup(pgid, signal)
  const deadline = performance.now() + STOP_GRACE_MS
  let killedAt: number | null = null
  while (liveMembers(pgid) > 0) {
    const now = performance.now()
    if (killedAt === null) {
      if (forced || now >= deadline) {
        log(`${item.name} did not stop in time; sending SIGKILL`)
        signalGroup(pgid, 'SIGKILL')
        killedAt = now
      }
    } else if (now - killedAt >= KILL_WAIT_MS) {
      log(`${item.name} (process group ${pgid}) could not be confirmed stopped`)
      return false
    }
    await sleep(POLL_MS)
  }
  return killedAt === null
}

/** Stops every owned group once; later calls share the first result (true = clean). */
function stopAll(signal: NodeJS.Signals): Promise<boolean> {
  const forward = signal === 'SIGINT' ? 'SIGINT' : 'SIGTERM'
  stopTask ??= Promise.all([...owned].map((item) => stopGroup(item, forward))).then((results) =>
    results.every(Boolean),
  )
  return stopTask
}

function onSignal(signal: NodeJS.Signals): void {
  if (stopSignal === null) {
    stopSignal = signal
    stopSignalAt = performance.now()
    log(`${signal} received; stopping child processes (press Ctrl+C again to force)`)
    requestStop(signal)
    void stopAll(signal)
    return
  }
  // Wrappers (timeout, bun run, process managers) may forward the same signal
  // right away; only a deliberate repeat forces the shutdown.
  if (performance.now() - stopSignalAt < REPEAT_IGNORE_MS) return
  if (!forced) {
    forced = true
    log('forcing shutdown')
    for (const item of owned) {
      if (item.pgid !== undefined) signalGroup(item.pgid, 'SIGKILL')
    }
  }
}

// ---- child processes ----

function launch(
  name: string,
  command: string,
  args: string[],
  options: { cwd: string; env?: NodeJS.ProcessEnv; pipeStdout?: boolean },
): Owned {
  const child = spawn(command, args, {
    cwd: options.cwd,
    env: options.env ?? process.env,
    detached: true,
    stdio: ['ignore', options.pipeStdout ? 'pipe' : 'inherit', 'inherit'],
  })
  const exited = new Promise<Exit>((done) => {
    child.once('error', (error) => done({ code: null, signal: null, error: error.message }))
    child.once('exit', (code, signal) => done({ code, signal }))
  })
  const item: Owned = { name, child, pgid: child.pid, exited }
  owned.add(item)
  return item
}

/** Waits for the leader to exit, then reaps any descendants still in its group. */
async function retire(item: Owned): Promise<Exit> {
  const exit = await item.exited
  await stopGroup(item, 'SIGTERM')
  owned.delete(item)
  return exit
}

function describe(exit: Exit): string {
  if (exit.error !== undefined) return `failed to start: ${exit.error}`
  return exit.signal !== null ? `killed by ${exit.signal}` : `exit code ${exit.code}`
}

// ---- build ----

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === 'object' && value !== null
}

/** Extracts the angelic-angel executable path from a cargo JSON message line. */
function executableFrom(line: string): string | null {
  if (!line.startsWith('{')) return null
  let message: unknown
  try {
    message = JSON.parse(line)
  } catch {
    return null
  }
  if (!isRecord(message) || message.reason !== 'compiler-artifact') return null
  const { target, executable } = message
  if (!isRecord(target) || target.name !== BIN_NAME || typeof executable !== 'string') return null
  if (!Array.isArray(target.kind) || !target.kind.includes('bin')) return null
  return executable
}

// Asking cargo for the artifact path honors CARGO_TARGET_DIR and build.target-dir.
async function build(): Promise<string | null> {
  log('cargo build --locked')
  const item = launch(
    'cargo',
    'cargo',
    ['build', '--locked', '--message-format=json-render-diagnostics'],
    { cwd: ROOT, pipeStdout: true },
  )
  let binary: string | null = null
  let pending = ''
  const take = (line: string): void => {
    binary = executableFrom(line) ?? binary
  }
  const stdout = item.child.stdout
  const drained = new Promise<void>((done) => {
    if (stdout === null) return done()
    stdout.setEncoding('utf8')
    stdout.on('data', (chunk: string) => {
      const lines = (pending + chunk).split('\n')
      pending = lines.pop() ?? ''
      for (const line of lines) take(line)
    })
    stdout.once('close', () => done())
  })

  const exit = await retire(item)
  await Promise.race([drained, sleep(2_000)])
  take(pending)

  if (exit.code !== 0) {
    if (stopSignal === null) log(`cargo build failed (${describe(exit)})`)
    return null
  }
  if (binary === null) {
    log(`cargo build finished but reported no ${BIN_NAME} executable`)
    return null
  }
  return binary
}

// ---- main ----

async function main(): Promise<number> {
  if (process.platform === 'win32') fail('this launcher needs a POSIX system (process groups)')

  const config = resolve(ROOT, process.env.DEV_CONFIG || `${BIN_NAME}.toml`)
  const apiPort = readPort('DEV_API_PORT', 8080)
  const webPort = readPort('DEV_WEB_PORT', 11475)
  if (apiPort === webPort) fail('DEV_API_PORT and DEV_WEB_PORT must differ')

  if (!existsSync(resolve(WEB_DIR, 'node_modules', '.bin', 'vite'))) {
    fail('web/ dependencies are missing; run `bun install --cwd web --frozen-lockfile` first')
  }

  const binary = await build()
  if (binary === null) return stopSignal === null ? 1 : 0
  if (stopSignal !== null) return 0

  log(`config: ${config}${existsSync(config) ? '' : ' (not created yet)'}`)
  log(
    'serve starts the real receiver when this config holds a registration; set DEV_CONFIG to use a scratch config',
  )
  log(
    `WebUI:  http://localhost:${webPort}/ (Vite on ${WEB_HOST}:${webPort}; local only, no authentication)`,
  )
  log(`API:    http://${API_HOST}:${apiPort}/ (proxied from Vite as /api)`)

  const api = launch(
    'api',
    binary,
    ['--config', config, 'serve', '--bind', `${API_HOST}:${apiPort}`],
    { cwd: ROOT },
  )
  const web = launch(
    'web',
    process.execPath,
    ['--bun', 'run', 'dev', '--host', WEB_HOST, '--port', String(webPort), '--strictPort'],
    {
      cwd: WEB_DIR,
      env: { ...process.env, VITE_API_TARGET: `http://${API_HOST}:${apiPort}` },
    },
  )

  const first = await Promise.race([
    api.exited.then((exit) => ({ item: api, exit })),
    web.exited.then((exit) => ({ item: web, exit })),
    stopRequested.then(() => null),
  ])
  let code = 0
  if (first !== null) {
    log(`${first.item.name} exited (${describe(first.exit)}); stopping the other process`)
    code = first.exit.code !== null && first.exit.code !== 0 ? first.exit.code : 1
  }
  const clean = await stopAll(stopSignal ?? 'SIGTERM')
  await Promise.all([api.exited, web.exited])
  owned.clear()
  if (!clean) code = 1
  log('stopped')
  return code
}

for (const signal of ['SIGINT', 'SIGTERM', 'SIGHUP'] as const) {
  process.on(signal, () => onSignal(signal))
}

let exitCode = 1
try {
  exitCode = await main()
} catch (error) {
  console.error(error)
} finally {
  // Finish any shutdown already in flight before leaving.
  await (stopTask ?? stopAll('SIGTERM')).catch(() => false)
}
process.exit(exitCode)
