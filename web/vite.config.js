import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

// The console is served at / (customer) and /ui/ (operator) by the suhms binary, so every URL
// in the build is relative. `npm run dev` proxies the APIs to a local suhms:
//   SUHMS_CONSOLE=http://127.0.0.1:9300 (customer)  or  SUHMS_ADMIN=http://127.0.0.1:9200 (operator)
const consoleTarget = process.env.SUHMS_CONSOLE;
const adminTarget = process.env.SUHMS_ADMIN;
const proxy = {};
if (adminTarget) {
  // The dev page is at /, so its relative api/ calls become /ui/api/ on the admin listener.
  proxy['/api'] = { target: adminTarget, rewrite: (p) => `/ui${p}` };
  proxy['/admin'] = adminTarget;
} else if (consoleTarget) {
  proxy['/api'] = consoleTarget;
  proxy['/s3'] = consoleTarget;
}

export default defineConfig({
  base: './',
  plugins: [svelte()],
  build: { outDir: 'dist', assetsDir: 'assets', sourcemap: false, target: 'es2022' },
  server: { proxy },
});
