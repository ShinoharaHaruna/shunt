import { createContext, useContext, useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { en } from './en'
import type { Dict, Key } from './en'
import { zhCN } from './zh-CN'

/** code → display label in that language (autonym). */
export const LANGS = { en: 'EN', 'zh-CN': '中文' } as const
export type Lang = keyof typeof LANGS

const dicts: Record<Lang, Dict> = { en, 'zh-CN': zhCN }
const KEY = 'shunt-lang'

/** stored choice → browser language list → 'en'. The stored value
 *  wins outright — a manual pick is a deliberate override. */
function detect(): Lang {
  try {
    const v = localStorage.getItem(KEY)
    if (v && v in LANGS) return v as Lang
  } catch {
    /* private mode — fall through to navigator */
  }
  const navs = navigator.languages ?? [navigator.language]
  for (const raw of navs) {
    const l = raw.toLowerCase()
    if (l in LANGS) return l as Lang
    if (l.startsWith('zh')) return 'zh-CN' // zh-* variants → the one zh dict
    const base = l.split('-')[0]
    if (base in LANGS) return base as Lang
  }
  return 'en'
}

export function fmtVars(text: string, vars?: Record<string, string | number>): string {
  if (!vars) return text
  return text.replace(/\{(\w+)\}/g, (m, k: string) => (k in vars ? String(vars[k]) : m))
}

const I18nCtx = createContext<{
  lang: Lang
  setLang: (l: Lang) => void
  t: (key: Key, vars?: Record<string, string | number>) => string
} | null>(null)

export function I18nProvider({ children }: { children: ReactNode }) {
  const [lang, setLangState] = useState<Lang>(detect)
  useEffect(() => {
    document.documentElement.lang = lang
    try {
      localStorage.setItem(KEY, lang)
    } catch {
      /* private mode — choice lives for the session only */
    }
  }, [lang])
  const t = (key: Key, vars?: Record<string, string | number>): string => {
    const text = dicts[lang][key] ?? en[key]
    // A missing key must stay findable — render it raw rather than blank.
    if (text === undefined) return key
    return fmtVars(text, vars)
  }
  return <I18nCtx.Provider value={{ lang, setLang: setLangState, t }}>{children}</I18nCtx.Provider>
}

export function useI18n() {
  const ctx = useContext(I18nCtx)
  if (!ctx) throw new Error('useI18n outside I18nProvider')
  return ctx
}
