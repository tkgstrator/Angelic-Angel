const POST_HOSTS = new Set(['x.com', 'www.x.com', 'twitter.com', 'www.twitter.com'])
const ICON_HOSTS = new Set(['pbs.twimg.com', 'abs.twimg.com'])

function allowed(value: string | null, hosts: ReadonlySet<string>): string | null {
  if (value === null) return null
  let url: URL
  try {
    url = new URL(value)
  } catch {
    return null
  }
  const plain = url.username === '' && url.password === '' && url.port === ''
  return url.protocol === 'https:' && plain && hosts.has(url.hostname) ? url.href : null
}

/** The notification payload is untrusted: only https links to X / Twitter become anchors. */
export function safePostUrl(value: string | null): string | null {
  return allowed(value, POST_HOSTS)
}

export function safeIconUrl(value: string | null): string | null {
  return allowed(value, ICON_HOSTS)
}
