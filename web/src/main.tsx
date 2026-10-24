import { StrictMode } from 'react'
import { createRoot } from 'react-dom/client'
import './index.css'
import { I18nProvider } from './i18n'
import App from './App'

// Theme fallback init — index.html's inline script does this
// pre-paint under the CSP hash; if that hash ever goes stale this
// catches the theme before React mounts (one frame late, not a
// wrong palette persisted).
try {
  document.documentElement.dataset.theme = localStorage.getItem('shunt-theme') || 'system'
} catch {
  /* private mode: attribute default 'system' stands */
}

const root = document.getElementById('root')
if (!root) throw new Error('#root missing')
createRoot(root).render(
  <StrictMode>
    <I18nProvider>
      <App />
    </I18nProvider>
  </StrictMode>,
)
