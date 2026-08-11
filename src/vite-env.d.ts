/// <reference types="vite/client" />

interface ImportMetaEnv {
  readonly VITE_JACKVOICE_VERSION?: string;
  readonly VITE_JACKVOICE_BUILD_ID?: string;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
