import { createRouter, createWebHistory } from 'vue-router'
import AppShell from '../components/layout/AppShell.vue'

// `/try` is the WASM-mode landing - same SPA, the boot path in
// `main.ts` swaps `api` to a WasmApiClient when the URL matches. The
// route exists so the Vue router doesn't render an empty view when
// users bookmark or refresh that path; rendering AppShell directly
// gives them the populated demo as soon as WASM finishes booting.
const router = createRouter({
  history: createWebHistory(),
  routes: [
    {
      path: '/',
      component: AppShell,
    },
    {
      path: '/try',
      component: AppShell,
    },
  ],
})

export default router
