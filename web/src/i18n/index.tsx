import { createContext, useContext, useEffect, useState } from 'react'
import type { ReactNode } from 'react'
import { en } from './en'
import type { Dict, Key } from './en'
import { fr } from './fr'
import { de } from './de'
import { it } from './it'
import { ja } from './ja'
import { es } from './es'
import { ko } from './ko'
import { zhCN } from './zh-CN'
import { zhTW } from './zh-TW'
import { ru } from './ru'
import { id } from './id'
import { ptBR } from './pt-BR'

/** code → display label in that language (autonym). Ordered like the
 *  README language switcher: English first, then the maintainer's order. */
export const LANGS = {
  en: 'English',
  fr: 'Français',
  de: 'Deutsch',
  it: 'Italiano',
  ja: '日本語',
  es: 'Español',
  ko: '한국어',
  'zh-CN': '简体中文',
  'zh-TW': '繁體中文',
  ru: 'Русский',
  id: 'Bahasa Indonesia',
  'pt-BR': 'Português (BR)',
} as const
export type Lang = keyof typeof LANGS

const dicts: Record<Lang, Dict> = {
  en,
  fr,
  de,
  it,
  ja,
  es,
  ko,
  'zh-CN': zhCN,
  'zh-TW': zhTW,
  ru,
  id,
  'pt-BR': ptBR,
}
const KEY = 'shunt-lang'

const TAGS = Object.keys(LANGS) as Lang[]

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
    const exact = TAGS.find((tag) => l === tag.toLowerCase())
    if (exact) return exact
    if (l.startsWith('zh')) {
      // Traditional-script regions → zh-TW, every other zh-* → zh-CN.
      return /hant|tw|hk|mo/.test(l) ? 'zh-TW' : 'zh-CN'
    }
    // Region-tagged picks (pt-BR) resolve through their base language.
    const base = l.split('-')[0]
    const sibling = TAGS.find((tag) => tag.toLowerCase().split('-')[0] === base)
    if (sibling) return sibling
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
