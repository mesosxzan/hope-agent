import { useState, useEffect, useCallback } from "react"
import { getTransport } from "@/lib/transport-provider"
import { parsePayload } from "@/lib/transport"

/** Light/dark mode preference */
export type ThemeMode = "auto" | "light" | "dark"

/** Color theme (accent palette). "default" = the original neutral theme. */
export type ColorTheme = "default" | "ocean" | "aurora" | "rose"

export const COLOR_THEMES: { id: ColorTheme; labelKey: string; preview: string }[] = [
  { id: "default", labelKey: "theme.colorDefault", preview: "hsl(0 0% 9%)" },
  { id: "ocean", labelKey: "theme.colorOcean", preview: "hsl(210 80% 38%)" },
  { id: "aurora", labelKey: "theme.colorAurora", preview: "hsl(270 60% 48%)" },
  { id: "rose", labelKey: "theme.colorRose", preview: "hsl(345 68% 44%)" },
]

function normalizeTheme(raw: string | null | undefined): ThemeMode {
  return raw === "light" || raw === "dark" ? raw : "auto"
}

function normalizeColorTheme(raw: string | null | undefined): ColorTheme {
  if (raw === "ocean" || raw === "aurora" || raw === "rose") return raw
  return "default"
}

/** Apply theme visually (DOM + native window) without persisting to config */
export function applyThemeVisual(mode: ThemeMode, colorTheme: ColorTheme = "default") {
  const root = document.documentElement
  let isDark: boolean
  if (mode === "dark") {
    isDark = true
  } else if (mode === "light") {
    isDark = false
  } else {
    isDark = window.matchMedia("(prefers-color-scheme: dark)").matches
  }

  if (isDark) {
    root.classList.add("dark")
  } else {
    root.classList.remove("dark")
  }

  // Apply color theme class
  root.classList.remove("theme-ocean", "theme-aurora", "theme-rose")
  if (colorTheme !== "default") {
    root.classList.add(`theme-${colorTheme}`)
  }

  // Sync inline background to prevent flash on resize
  root.style.backgroundColor = isDark ? "#0f0f0f" : "#ffffff"
  root.style.colorScheme = isDark ? "dark" : "light"
  // Sync macOS NSWindow background color to match theme
  getTransport().call("set_window_theme", { isDark }).catch(() => {})
}

/** Apply theme visually and persist to backend config */
export function setThemePreference(mode: ThemeMode, colorTheme: ColorTheme = "default") {
  applyThemeVisual(mode, colorTheme)
  getTransport().call("set_theme", { theme: mode }).catch(() => {})
  getTransport().call("set_color_theme", { colorTheme }).catch(() => {})
}

/** Load saved theme from backend config and apply it visually. */
export async function initThemeFromConfig(): Promise<{ mode: ThemeMode; colorTheme: ColorTheme }> {
  try {
    const [stored, storedColor] = await Promise.all([
      getTransport().call<string>("get_theme"),
      getTransport().call<string>("get_color_theme").catch(() => "default"),
    ])
    const mode = normalizeTheme(stored)
    const colorTheme = normalizeColorTheme(storedColor)
    applyThemeVisual(mode, colorTheme)
    return { mode, colorTheme }
  } catch {
    applyThemeVisual("auto")
    return { mode: "auto", colorTheme: "default" }
  }
}

/** Listen for backend theme changes and keep DOM/native window in sync. */
export function listenThemeConfigChange(
  onChange?: (mode: ThemeMode, colorTheme: ColorTheme) => void,
): () => void {
  return getTransport().listen("config:changed", (raw) => {
    try {
      const payload = parsePayload<{ category?: string }>(raw)
      if (payload?.category === "theme") {
        Promise.all([
          getTransport().call<string>("get_theme"),
          getTransport().call<string>("get_color_theme").catch(() => "default"),
        ]).then(([stored, storedColor]) => {
          const mode = normalizeTheme(stored)
          const colorTheme = normalizeColorTheme(storedColor)
          onChange?.(mode, colorTheme)
          applyThemeVisual(mode, colorTheme)
        }).catch(() => {})
      }
    } catch {
      /* ignore parse errors */
    }
  })
}

export function useTheme() {
  const [theme, setThemeState] = useState<ThemeMode>("auto")
  const [colorTheme, setColorThemeState] = useState<ColorTheme>("default")

  // Load theme from backend config.json on mount (apply visually only, no write-back)
  useEffect(() => {
    initThemeFromConfig().then(({ mode, colorTheme: ct }) => {
      setThemeState(mode)
      setColorThemeState(ct)
    }).catch(() => {})
  }, [])

  const setTheme = useCallback((mode: ThemeMode) => {
    setThemeState(mode)
    setThemePreference(mode, colorTheme)
  }, [colorTheme])

  const setColorTheme = useCallback((ct: ColorTheme) => {
    setColorThemeState(ct)
    setThemePreference(theme, ct)
  }, [theme])

  // Listen for config changes from backend (e.g. ha-settings skill updates theme)
  useEffect(() => {
    return listenThemeConfigChange((mode, ct) => {
      setThemeState(mode)
      setColorThemeState(ct)
    })
  }, [])

  // Listen for system changes when in "auto" mode
  useEffect(() => {
    const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)")
    const handleChange = () => {
      if (theme === "auto") {
        applyThemeVisual("auto", colorTheme)
      }
    }

    mediaQuery.addEventListener("change", handleChange)
    return () => mediaQuery.removeEventListener("change", handleChange)
  }, [theme, colorTheme])

  // Cycle through modes: auto → light → dark → auto
  const cycleTheme = useCallback(() => {
    setTheme(theme === "auto" ? "light" : theme === "light" ? "dark" : "auto")
  }, [theme, setTheme])

  return { theme, setTheme, cycleTheme, colorTheme, setColorTheme }
}
