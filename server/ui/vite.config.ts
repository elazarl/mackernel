import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// Built to dist/ and embedded into the Rust binary (rust-embed). In dev,
// `npm run dev` proxies /api to the running server on :8087.
export default defineConfig({
  plugins: [
    tailwindcss(),
    // React Compiler auto-memoizes — the old manual useMemo/useRef gymnastics are gone.
    react({ babel: { plugins: [["babel-plugin-react-compiler", { target: "19" }]] } }),
  ],
  build: { outDir: "dist", emptyOutDir: true },
  server: {
    // Allow importing the spec from the repo's docs/ (outside the ui root) via ?raw.
    fs: { allow: ["..", "../..", "../../.."] },
    proxy: {
      "/api": { target: "http://127.0.0.1:8087", changeOrigin: true },
    },
  },
});
