import { useTranslation } from "react-i18next"
import { cn } from "@/lib/utils"
import { useTheme, type ThemeMode, COLOR_THEMES } from "@/hooks/useTheme"
import { Button } from "@/components/ui/button"
import { Monitor, Sun, Moon, Check } from "lucide-react"

const THEME_OPTIONS: {
  mode: ThemeMode
  icon: React.ReactNode
  labelKey: string
  descKey: string
}[] = [
  {
    mode: "auto",
    icon: <Monitor className="h-5 w-5" />,
    labelKey: "theme.auto",
    descKey: "theme.autoDesc",
  },
  {
    mode: "light",
    icon: <Sun className="h-5 w-5" />,
    labelKey: "theme.light",
    descKey: "theme.lightDesc",
  },
  {
    mode: "dark",
    icon: <Moon className="h-5 w-5" />,
    labelKey: "theme.dark",
    descKey: "theme.darkDesc",
  },
]

export default function AppearancePanel() {
  const { t } = useTranslation()
  const { theme, setTheme, colorTheme, setColorTheme } = useTheme()

  return (
    <div className="flex-1 overflow-y-auto p-6">
      <h2 className="text-lg font-semibold text-foreground mb-1">{t("settings.appearance")}</h2>
      <p className="text-xs text-muted-foreground mb-5">{t("settings.appearanceDesc")}</p>

      <div className="space-y-1">
        {THEME_OPTIONS.map((opt) => (
          <Button
            key={opt.mode}
            variant="ghost"
            className={cn(
              "h-auto w-full justify-start gap-3 px-3 py-3 rounded-lg text-sm",
              theme === opt.mode
                ? "bg-primary/10 text-primary font-medium hover:bg-primary/10 hover:text-primary"
                : "text-foreground hover:bg-secondary/60",
            )}
            onClick={() => setTheme(opt.mode)}
          >
            <span
              className={cn(
                "shrink-0",
                theme === opt.mode ? "text-primary" : "text-muted-foreground",
              )}
            >
              {opt.icon}
            </span>
            <div className="flex-1 text-left">
              <div>{t(opt.labelKey)}</div>
              <div className="text-xs text-muted-foreground font-normal">{t(opt.descKey)}</div>
            </div>
            {theme === opt.mode && <Check className="h-4 w-4 text-primary shrink-0" />}
          </Button>
        ))}
      </div>

      {/* Color theme selector */}
      <div className="mt-6">
        <h3 className="text-sm font-semibold text-foreground mb-1">{t("theme.colorScheme", "配色方案")}</h3>
        <p className="text-xs text-muted-foreground mb-3">{t("theme.colorSchemeDesc", "选择界面配色风格")}</p>
        <div className="grid grid-cols-4 gap-2">
          {COLOR_THEMES.map((ct) => (
            <button
              key={ct.id}
              className={cn(
                "flex flex-col items-center gap-1.5 rounded-lg border-2 p-2.5 transition-all",
                colorTheme === ct.id
                  ? "border-primary bg-primary/5"
                  : "border-transparent bg-secondary/40 hover:bg-secondary/70",
              )}
              onClick={() => setColorTheme(ct.id)}
            >
              <span
                className="h-6 w-6 rounded-full border shadow-sm"
                style={{ backgroundColor: ct.preview }}
              />
              <span className={cn(
                "text-[0.6875rem] leading-tight",
                colorTheme === ct.id ? "text-primary font-medium" : "text-muted-foreground",
              )}>
                {t(ct.labelKey, ct.id === "default" ? "默认" : ct.id === "ocean" ? "海洋" : ct.id === "aurora" ? "极光" : "玫瑰")}
              </span>
            </button>
          ))}
        </div>
      </div>
    </div>
  )
}
