import reactHooks from "eslint-plugin-react-hooks";
import tsParser from "@typescript-eslint/parser";

// Rules of Hooks + exhaustive-deps — the two battle-tested hook rules. tsc handles
// type-checking; we don't lint style here. v7 also ships compiler-era rules
// (set-state-in-effect, static-components, refs) — opinionated and noisy on correct
// code, so they're off. Swap the rules block for `...reactHooks.configs.flat["recommended-latest"]`
// to opt into the full set.
export default [
  { ignores: ["dist/"] },
  {
    files: ["src/**/*.{ts,tsx}"],
    plugins: { "react-hooks": reactHooks },
    languageOptions: { parser: tsParser, parserOptions: { ecmaFeatures: { jsx: true } } },
    rules: {
      "react-hooks/rules-of-hooks": "error",
      "react-hooks/exhaustive-deps": "warn",
    },
  },
];
