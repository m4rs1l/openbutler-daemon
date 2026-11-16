// The door's peer: announce, dispatch commands, own one AbortController
// per in-flight call. Concurrent calls run side by side — the host caps
// concurrency, this file never serializes them.

import { createRequire } from "node:module";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";

import { PROVIDER_KEYS } from "./providers.mjs";
import { readCommands, writeEvent } from "./framing.mjs";
import { classify, runCall } from "./runner.mjs";

const PROTOCOL_VERSION = 1;
const controllers = new Map();

/// The exact installed `ai` version — the handshake pins it because
/// message formats move between releases. Walks up from the resolved
/// entry to the package root.
function aiVersion() {
  try {
    const require = createRequire(import.meta.url);
    let dir = dirname(require.resolve("ai"));
    for (;;) {
      try {
        const pkg = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
        if (pkg.name === "ai") return String(pkg.version);
      } catch {
        // keep walking up
      }
      const parent = dirname(dir);
      if (parent === dir) break;
      dir = parent;
    }
  } catch {
    // fall through
  }
  return "unknown";
}

function invalidRequest(id, message) {
  return {
    type: "error",
    id,
    kind: "invalid-request",
    retryable: false,
    status: null,
    retryAfterMs: null,
    message,
  };
}

async function handleCall(call) {
  if (
    typeof call?.id !== "string" ||
    typeof call?.model !== "string" ||
    typeof call?.provider?.key !== "string" ||
    !Array.isArray(call.messages)
  ) {
    if (typeof call?.id === "string") {
      writeEvent(invalidRequest(call.id, "malformed call command"));
    } else {
      process.stderr.write("openbutler sidecar: call without an id ignored\n");
    }
    return;
  }
  const controller = new AbortController();
  controllers.set(call.id, controller);
  try {
    await runCall(call, { signal: controller.signal, emit: writeEvent });
  } catch (error) {
    // Setup failures (factory throw, bad schema) land here; an abort is
    // the host's own cancel and answers nothing.
    if (!controller.signal.aborted) {
      writeEvent({ type: "error", id: call.id, ...classify(error) });
    }
  } finally {
    controllers.delete(call.id);
  }
}

export function start() {
  writeEvent({
    type: "ready",
    protocolVersion: PROTOCOL_VERSION,
    aiSdkVersion: aiVersion(),
    providers: PROVIDER_KEYS,
  });

  readCommands((command) => {
    if (command?.type === "cancel" && typeof command.id === "string") {
      controllers.get(command.id)?.abort();
      return;
    }
    if (command?.type === "call") {
      void handleCall(command);
      return;
    }
    process.stderr.write("openbutler sidecar: unknown command type ignored\n");
  });
}
