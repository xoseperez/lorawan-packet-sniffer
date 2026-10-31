/// <reference types="vite/client" />

// The version from package.json, inserted by Vite at build time (see
// `define` in vite.config.ts). A Rust test keeps package.json in step with
// the binary's version, so the footer shows the version of the running sniffer.
declare const __APP_VERSION__: string;
