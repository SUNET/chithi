/// <reference types="vite/client" />

declare module "*.vue" {
  import type { DefineComponent } from "vue";
  const component: DefineComponent<object, object, unknown>;
  export default component;
}

/** Injected by Vite's `define`; sourced from package.json at build time. */
declare const __APP_VERSION__: string;
