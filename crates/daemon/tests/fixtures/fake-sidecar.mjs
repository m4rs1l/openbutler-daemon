// Fake sidecar for the door tests: real NDJSON over stdin/stdout,
// behavior keyed by the call id prefix. Never part of production.
import { appendFileSync } from "node:fs";

const MODE = process.env.FAKE_MODE ?? "default";
const PROVIDERS = JSON.parse(process.env.FAKE_PROVIDERS ?? '["ok"]');
const LOG = process.env.FAKE_LOG;

const active = new Map();
let buffer = "";

function emit(object) {
  process.stdout.write(JSON.stringify(object) + "\n");
}

if (MODE !== "no-ready") {
  if (MODE === "wrong-version") {
    emit({ type: "ready", protocolVersion: 99, aiSdkVersion: "0.0.0", providers: PROVIDERS });
  } else {
    emit({ type: "ready", protocolVersion: 1, aiSdkVersion: "7.0.173", providers: PROVIDERS });
  }
}

process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let index;
  while ((index = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, index);
    buffer = buffer.slice(index + 1);
    if (line.trim() === "") continue;
    let command;
    try {
      command = JSON.parse(line);
    } catch {
      emit({ type: "log-only", note: "bad command suppressed" });
      continue;
    }
    if (command.type === "cancel") {
      const timer = active.get(command.id);
      if (timer) {
        clearInterval(timer);
        active.delete(command.id);
      }
      continue;
    }
    if (command.type !== "call") continue;
    if (LOG) appendFileSync(LOG, command.id + "\n");
    const id = command.id;
    const behavior = id.split(":")[0];
    switch (behavior) {
      case "hang":
        break;
      case "abort":
        break;
      case "chatty": {
        let n = 0;
        const timer = setInterval(() => emit({ type: "text-delta", id, delta: `t${n++}` }), 60);
        active.set(id, timer);
        break;
      }
      case "bad-line":
        process.stdout.write("this is not json\n");
        break;
      case "big-line":
        process.stdout.write(
          JSON.stringify({ type: "text-delta", id, delta: "x".repeat(200000) }) + "\n",
        );
        break;
      case "unknown":
        emit({ type: "future-event", id });
        emit({ type: "text-delta", id, delta: "after-unknown" });
        emit({ type: "finish", id, finishReason: "stop" });
        break;
      case "error":
        emit({
          type: "error",
          id,
          kind: "rate-limit",
          retryable: true,
          status: 429,
          retryAfterMs: 1500,
          message: "slow down",
        });
        break;
      case "length":
        emit({ type: "text-delta", id, delta: "partial summary" });
        emit({ type: "usage", id, promptTokens: 10, completionTokens: 5, totalTokens: 15 });
        emit({ type: "finish", id, finishReason: "length" });
        break;
      case "tool":
        emit({ type: "tool-call", id, name: "grep", arguments: {}, callId: "c1" });
        emit({ type: "usage", id, promptTokens: 10, completionTokens: 5, totalTokens: 15 });
        emit({ type: "finish", id, finishReason: "tool_calls" });
        break;
      case "empty":
        emit({ type: "usage", id, promptTokens: 10, completionTokens: 0, totalTokens: 10 });
        emit({ type: "finish", id, finishReason: "stop" });
        break;
      case "echo": {
        const messages = command.messages ?? [];
        const last = messages[messages.length - 1];
        const content = typeof last?.content === "string" ? last.content : "";
        const probe = content.includes("one-time recovery from a full context window")
          ? "recovery-suffix-present"
          : "recovery-suffix-missing";
        emit({ type: "text-delta", id, delta: probe });
        emit({ type: "finish", id, finishReason: "stop" });
        break;
      }
      default:
        emit({ type: "text-delta", id, delta: "Hello" });
        emit({ type: "text-delta", id, delta: " world" });
        emit({ type: "usage", id, promptTokens: 10, completionTokens: 5, totalTokens: 15 });
        emit({ type: "finish", id, finishReason: "stop" });
    }
  }
});
process.stdin.on("end", () => process.exit(0));
