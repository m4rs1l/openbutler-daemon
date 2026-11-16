// Bundle the sidecar into dist/: entry + one lazy chunk per provider.
// Dynamic `import()` in the registry is what gives each provider its own
// chunk — the bundle-half of M2b criterion 3 (lazy per-provider load).
import { build } from "esbuild";
import { writeFileSync } from "node:fs";

const result = await build({
  entryPoints: ["src/entry.mjs"],
  bundle: true,
  platform: "node",
  format: "esm",
  target: "node20",
  splitting: true,
  outdir: "dist",
  outbase: "src",
  entryNames: "[name]",
  chunkNames: "chunks/[name]-[hash]",
  metafile: true,
  // CJS dependencies inside the bundle still call require() for node
  // builtins; the banner hands them one that resolves from the output
  // file (esbuild's canonical ESM fix).
  banner: {
    js: "import { createRequire as __obRequire } from 'node:module';\nconst require = __obRequire(import.meta.url);",
  },
  // Stack traces and first-build debugging stay readable; minification
  // can return when the packaging slice measures what it saves.
  minify: false,
  logLevel: "warning",
});

writeFileSync("dist/metafile.json", JSON.stringify(result.metafile, null, 1));
const outputs = Object.keys(result.metafile.outputs).filter((name) => name.endsWith(".js"));
console.log(`bundled ${outputs.length} js outputs:\n  ${outputs.join("\n  ")}`);
