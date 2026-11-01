export default defineNuxtConfig({
  ssr: false,
  devtools: { enabled: false },
  css: ["~/assets/css/main.css"],
  compatibilityDate: "2025-07-15",
  typescript: { strict: true, typeCheck: false },
});
