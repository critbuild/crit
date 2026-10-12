import { defineConfig } from 'astro/config';

export default defineConfig({
  output: 'static',
  trailingSlash: 'always',
  server: { port: 4321 },
  devToolbar: { enabled: false },
});
