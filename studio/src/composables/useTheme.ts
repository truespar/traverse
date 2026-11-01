import { computed } from 'vue'
import { useSettingsStore } from '../stores/settings'

type Theme = 'light' | 'dark'

export function useTheme() {
  const settings = useSettingsStore()

  const theme = computed({
    get: () => settings.theme,
    set: (v: Theme) => { settings.theme = v },
  })

  const setTheme = (newTheme: Theme) => {
    if (newTheme !== 'light' && newTheme !== 'dark') newTheme = 'dark'
    settings.theme = newTheme
    document.documentElement.setAttribute('data-theme', newTheme)
    document.documentElement.setAttribute('data-ag-theme-mode', newTheme)
  }

  const toggleTheme = () => {
    const newTheme: Theme = settings.theme === 'dark' ? 'light' : 'dark'
    setTheme(newTheme)
    return newTheme
  }

  const initTheme = () => {
    document.documentElement.setAttribute('data-theme', settings.theme)
    document.documentElement.setAttribute('data-ag-theme-mode', settings.theme)
  }

  return { theme, setTheme, toggleTheme, initTheme }
}
