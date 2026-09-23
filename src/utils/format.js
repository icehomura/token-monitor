const UNIT_KEY = 'tm_unit_convert'

export function getConvertUnits() {
  return localStorage.getItem(UNIT_KEY) === '1'
}

export function setConvertUnits(v) {
  localStorage.setItem(UNIT_KEY, v ? '1' : '0')
}

export function fmtTokens(n, convertUnits) {
  if (!convertUnits) return n.toLocaleString()
  if (n >= 1e9) return (n / 1e9).toFixed(1) + 'B'
  if (n >= 1e6) return (n / 1e6).toFixed(1) + 'M'
  if (n >= 1e3) return (n / 1e3).toFixed(1) + 'K'
  return String(n)
}
