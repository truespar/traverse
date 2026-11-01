/// <reference types="vite/client" />

declare module '*.vue' {
  import type { DefineComponent } from 'vue'
  const component: DefineComponent<{}, {}, any>
  export default component
}

// Side-effect-only CSS imports (no type declarations of their own).
declare module '@fontsource-variable/inter'
declare module '@fontsource-variable/jetbrains-mono'
