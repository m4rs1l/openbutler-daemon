// NDJSON over stdin/stdout: the only reader of stdin and the only
// writer of stdout. One JSON value per line; the writable's queue keeps
// events in order.

let buffer = "";

export function writeEvent(event) {
  process.stdout.write(`${JSON.stringify(event)}\n`);
}

/// Read commands and hand each to `onCommand` without awaiting it: a
/// slow call must never delay the next line (the host multiplexes by
/// call id, concurrency capped host-side).
export function readCommands(onCommand) {
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
        process.stderr.write("openbutler sidecar: non-JSON command ignored\n");
        continue;
      }
      onCommand(command);
    }
  });
  // The portable orphan rule: stdin EOF means the host is gone. Exit
  // only after stdout has drained — a plain process.exit() would drop
  // buffered protocol lines (the ready handshake on a quick close).
  process.stdin.on("end", () => {
    process.stdout.write("", () => process.exit(0));
  });
}
