import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Port fixe : Tauri charge http://localhost:5173 en mode développement
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
    // Ne pas surveiller la partie Rust : Windows verrouille les fichiers pendant la compilation
    watch: { ignored: ["**/src-tauri/**"] },
  },
  build: { target: "es2021", outDir: "dist" },
});