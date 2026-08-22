import { createServer } from "node:net";

const host = "127.0.0.1";
const port = Number.parseInt(process.env.REDIS_PORT ?? "6390", 10);
const maximumBulkLength = 536_870_912;
const maximumParts = 1_024;
const decoder = new TextDecoder("utf-8", { fatal: true });
const database = new Map();

function lineEnd(input, offset) {
  return input.indexOf("\r\n", offset, "ascii");
}

function unsignedInteger(input, start, end) {
  if (start === end) throw new Error("empty RESP length");
  let value = 0;
  for (let index = start; index < end; index += 1) {
    const byte = input[index];
    if (byte < 48 || byte > 57) throw new Error("invalid RESP length");
    value = value * 10 + byte - 48;
    if (!Number.isSafeInteger(value)) throw new Error("RESP length overflow");
  }
  return value;
}

function parseCommand(input) {
  if (input.length === 0) return undefined;
  if (input[0] !== 42) throw new Error("expected RESP array");
  const countEnd = lineEnd(input, 1);
  if (countEnd < 0) return undefined;
  const count = unsignedInteger(input, 1, countEnd);
  if (count === 0 || count > maximumParts)
    throw new Error("invalid RESP array length");

  const parts = [];
  let offset = countEnd + 2;
  for (let index = 0; index < count; index += 1) {
    if (offset >= input.length) return undefined;
    if (input[offset] !== 36) throw new Error("expected RESP bulk string");
    const lengthEnd = lineEnd(input, offset + 1);
    if (lengthEnd < 0) return undefined;
    const length = unsignedInteger(input, offset + 1, lengthEnd);
    if (length > maximumBulkLength) throw new Error("RESP frame too large");
    const payloadStart = lengthEnd + 2;
    const payloadEnd = payloadStart + length;
    if (payloadEnd + 2 > input.length) return undefined;
    if (input[payloadEnd] !== 13 || input[payloadEnd + 1] !== 10)
      throw new Error("bulk string lacks CRLF");
    parts.push(decoder.decode(input.subarray(payloadStart, payloadEnd)));
    offset = payloadEnd + 2;
  }
  return { parts, consumed: offset };
}

function execute(parts) {
  const command = parts[0].toUpperCase();
  if (command === "PING") return "+PONG\r\n";
  if (command === "SET") {
    if (parts.length < 3) return "-ERR SET requires a key and value\r\n";
    database.set(parts[1], parts[2]);
    return "+OK\r\n";
  }
  if (command === "GET") {
    if (parts.length < 2) return "-ERR GET requires a key\r\n";
    const value = database.get(parts[1]);
    if (value === undefined) return "$-1\r\n";
    return `$${Buffer.byteLength(value)}\r\n${value}\r\n`;
  }
  if (command === "DEL") {
    if (parts.length < 2) return "-ERR DEL requires a key\r\n";
    return database.delete(parts[1]) ? ":1\r\n" : ":0\r\n";
  }
  return "-ERR unknown command\r\n";
}

const server = createServer((socket) => {
  let input = Buffer.alloc(0);
  socket.on("data", (chunk) => {
    input = input.length === 0 ? chunk : Buffer.concat([input, chunk]);
    try {
      while (input.length > 0) {
        const parsed = parseCommand(input);
        if (parsed === undefined) return;
        socket.write(execute(parsed.parts));
        input = input.subarray(parsed.consumed);
      }
    } catch {
      socket.destroy();
    }
  });
  socket.on("error", () => {});
});

server.listen({ host, port });
