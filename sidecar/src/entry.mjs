// The sidecar entry point. Protocol stdout belongs to the door, so every
// console channel is redirected to stderr BEFORE anything else runs —
// dynamic import after the redirect guarantees ordering regardless of
// bundler behavior.

for (const method of ["log", "info", "debug", "warn", "error", "trace"]) {
  console[method] = (...args) => {
    process.stderr.write(`${args.map(String).join(" ")}\n`);
  };
}

const { start } = await import("./main.mjs");
start();
