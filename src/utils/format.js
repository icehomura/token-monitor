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

function currencySymbol(code) {
  switch (String(code || '').toUpperCase()) {
    case 'USD': return '$'
    case 'CNY': return '¥'
    default: return ''
  }
}

/**
 * 余额展示：币种符号 + 金额。
 *
 * 币种来自后端响应（DeepSeek 为 balance_infos.currency，sub2api 为 unit），
 * 未识别的币种不硬套符号，退回「金额 + 币种码」，避免把 EUR 显示成 ¥。
 * 金额是后端定型的字符串，这里只做拼接，不做浮点转换。
 */
export function fmtBalance(currency, total) {
  const amount = (total === null || total === undefined || total === '') ? '--' : String(total)
  const sym = currencySymbol(currency)
  if (sym) return `${sym}${amount}`
  return currency ? `${amount} ${String(currency).toUpperCase()}` : amount
}
