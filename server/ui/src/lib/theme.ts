// Theme lives in localStorage and is reflected as data-theme on <html>; the CSS
// palette (index.css) is driven by variables that a :root[data-theme="light"]
// block overrides. applyTheme runs once at module load so the unlock gate is
// themed too.
export type Theme = "dark" | "light";
const THEME_KEY = "mk-theme";

export const getTheme = (): Theme =>
  localStorage.getItem(THEME_KEY) === "light" ? "light" : "dark";

export const applyTheme = (t: Theme) => {
  document.documentElement.dataset.theme = t;
};

export const setTheme = (t: Theme) => {
  applyTheme(t);
  localStorage.setItem(THEME_KEY, t);
};

applyTheme(getTheme());
