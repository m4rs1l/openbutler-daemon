// One call, start to finish: resolve the provider, run a single SDK
// step, emit the door's event shapes. The error taxonomy here is the
// classification the host's retry policy reads.

import { APICallError, isStepCount, streamText } from "ai";
import { jsonSchema } from "@ai-sdk/provider-utils";

import { resolveModel } from "./providers.mjs";

function retryAfterMs(headers) {
  if (!headers) return null;
  const value = headers["retry-after"] ?? headers["Retry-After"];
  if (value == null) return null;
  const seconds = Number(value);
  if (Number.isFinite(seconds)) return Math.round(seconds * 1000);
  const date = Date.parse(value);
  if (!Number.isNaN(date)) return Math.max(0, date - Date.now());
  return null;
}

function overflowish(error) {
  const text = `${error.message ?? ""} ${error.responseBody ?? ""}`;
  return /context length|context window|maximum context|too many tokens/i.test(text);
}

/// Map a thrown/streamed error onto the door's taxonomy. Unknown kinds
/// classify as `other` — a newer failure mode must not break the door.
export function classify(error) {
  if (APICallError.isInstance(error)) {
    const status = error.statusCode;
    let kind = "other";
    if (status === 401 || status === 403) {
      kind = "auth";
    } else if (status === 429) {
      kind = "rate-limit";
    } else if (status === 400 || status === 422) {
      kind = overflowish(error) ? "context-overflow" : "invalid-request";
    } else if (status === 408 || status === undefined || status >= 500) {
      kind = "network";
    }
    return {
      kind,
      retryable: error.isRetryable,
      status: status ?? null,
      retryAfterMs: retryAfterMs(error.responseHeaders),
      message: error.message,
    };
  }
  if (error?.name === "AbortError") {
    return {
      kind: "aborted",
      retryable: false,
      status: null,
      retryAfterMs: null,
      message: "aborted",
    };
  }
  return {
    kind: "other",
    retryable: false,
    status: null,
    retryAfterMs: null,
    message: String(error?.message ?? error),
  };
}

export async function runCall(call, { signal, emit }) {
  const resolved = await resolveModel(call);
  if (resolved.error) {
    emit({
      type: "error",
      id: call.id,
      kind: "invalid-request",
      retryable: false,
      status: null,
      retryAfterMs: null,
      message: resolved.error,
    });
    return;
  }

  // Tools are schemas only — no `execute`, so the SDK cannot run its
  // own loop: it emits the call and stops, the host continues.
  const tools = {};
  for (const tool of call.tools ?? []) {
    tools[tool.name] = {
      ...(tool.description ? { description: tool.description } : {}),
      inputSchema: jsonSchema(tool.parameters),
    };
  }

  const result = streamText({
    model: resolved.model,
    messages: call.messages,
    ...(call.system != null && { system: call.system }),
    ...(Object.keys(tools).length > 0 && { tools }),
    ...(call.maxOutputTokens != null && { maxOutputTokens: call.maxOutputTokens }),
    ...(call.providerOptions != null && { providerOptions: call.providerOptions }),
    // Retries belong to the host: never inside the stream.
    maxRetries: 0,
    abortSignal: signal,
    // One step per door call — the host issues the next call itself.
    stopWhen: isStepCount(1),
  });

  for await (const part of result.fullStream) {
    switch (part.type) {
      case "text-delta":
        emit({ type: "text-delta", id: call.id, delta: part.text });
        break;
      case "reasoning-delta": {
        // Anthropic delivers the thinking signature as a separate
        // reasoning-delta with provider metadata and empty text.
        const signature = part.providerMetadata?.anthropic?.signature;
        if (part.text !== "") {
          emit({ type: "reasoning-delta", id: call.id, delta: part.text });
        }
        if (signature) {
          emit({ type: "reasoning-delta", id: call.id, delta: "", signature });
        }
        break;
      }
      case "tool-call":
        emit({
          type: "tool-call",
          id: call.id,
          name: part.toolName,
          arguments: part.input,
          callId: part.toolCallId,
        });
        break;
      case "finish": {
        const usage = part.totalUsage;
        emit({
          type: "usage",
          id: call.id,
          promptTokens: usage.inputTokens ?? null,
          completionTokens: usage.outputTokens ?? null,
          reasoningTokens: usage.outputTokenDetails?.reasoningTokens ?? null,
          totalTokens: usage.totalTokens ?? null,
        });
        emit({
          type: "finish",
          id: call.id,
          finishReason: part.finishReason ?? null,
        });
        break;
      }
      case "error":
        emit({ type: "error", id: call.id, ...classify(part.error) });
        return; // a structured error ends the call: no finish after it
      case "abort":
        return; // cancelled: the host dropped the route already
      default:
        break; // start/end/ping/raw/tool-input chunks: not door events
    }
  }
}
