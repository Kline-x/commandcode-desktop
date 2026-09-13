import { useEffect, useState } from "react";

export type ThemeMode = "system" | "light" | "dark";
export type EffectiveTheme = "light" | "dark";

const STORAGE_KEY = "commandcode-theme-mode";

/** 获取当前系统色彩偏好 */
export function getSystemTheme(): EffectiveTheme {
  if (typeof window === "undefined" || !window.matchMedia) {
    return "dark";
  }
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** 计算当前生效的主题（深色或浅色） */
export function resolveEffectiveTheme(mode: ThemeMode): EffectiveTheme {
  if (mode === "system") {
    return getSystemTheme();
  }
  return mode;
}

/** 将主题类与属性应用到 document.documentElement */
export function applyTheme(mode: ThemeMode): EffectiveTheme {
  const effective = resolveEffectiveTheme(mode);
  const root = document.documentElement;
  root.setAttribute("data-theme", effective);
  root.style.colorScheme = effective;
  return effective;
}

/** 初始化读取存储的主题设置，默认为 'system' */
export function getInitialThemeMode(): ThemeMode {
  if (typeof window === "undefined") {
    return "system";
  }
  const saved = localStorage.getItem(STORAGE_KEY) as ThemeMode | null;
  if (saved === "system" || saved === "light" || saved === "dark") {
    return saved;
  }
  return "system";
}

/** React Hook：管理主题模式与当前有效色彩，响应系统动态切换 */
export function useTheme(): {
  mode: ThemeMode;
  effectiveTheme: EffectiveTheme;
  setMode: (mode: ThemeMode) => void;
  toggleNext: () => void;
} {
  const [mode, setModeState] = useState<ThemeMode>(() => getInitialThemeMode());
  const [effectiveTheme, setEffectiveTheme] = useState<EffectiveTheme>(() =>
    resolveEffectiveTheme(getInitialThemeMode()),
  );

  const setMode = (newMode: ThemeMode) => {
    setModeState(newMode);
    try {
      localStorage.setItem(STORAGE_KEY, newMode);
    } catch {
      // 忽略存储异常
    }
    const resolved = applyTheme(newMode);
    setEffectiveTheme(resolved);
  };

  const toggleNext = () => {
    // 循环切换：跟随系统 -> 浅色模式 -> 深色模式
    if (mode === "system") {
      setMode("light");
    } else if (mode === "light") {
      setMode("dark");
    } else {
      setMode("system");
    }
  };

  useEffect(() => {
    // 初始设置生效
    const resolved = applyTheme(mode);
    setEffectiveTheme(resolved);

    // 监听系统色彩变化
    const mediaQuery = window.matchMedia("(prefers-color-scheme: dark)");
    const handleChange = () => {
      if (mode === "system") {
        const sys = getSystemTheme();
        document.documentElement.setAttribute("data-theme", sys);
        document.documentElement.style.colorScheme = sys;
        setEffectiveTheme(sys);
      }
    };

    mediaQuery.addEventListener("change", handleChange);
    return () => mediaQuery.removeEventListener("change", handleChange);
  }, [mode]);

  return { mode, effectiveTheme, setMode, toggleNext };
}
