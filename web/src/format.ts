export function formatTime(unixSeconds: number | null): string {
  if (unixSeconds === null) return '—'
  return new Date(unixSeconds * 1000).toLocaleString('ja-JP')
}

export function formatCount(n: number): string {
  return n.toLocaleString('ja-JP')
}
