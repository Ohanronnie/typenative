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

function reply(payload, close = false) {
  return { payload, close };
}

function bulk(value) {
  return `$${Buffer.byteLength(value)}\r\n${value}\r\n`;
}

function integer(value) {
  return `:${value}\r\n`;
}

function purgeExpired(key) {
  const record = database.get(key);
  if (record !== undefined && record.expiresAt !== undefined && record.expiresAt <= Date.now()) {
    database.delete(key);
    return undefined;
  }
  return record;
}

function execute(parts) {
  const command = parts[0].toUpperCase();
  if (command === "ECHO") {
    return parts.length < 2
      ? reply("-ERR ECHO requires a message\r\n")
      : reply(bulk(parts[1]));
  }
  if (command === "PING")
    return parts.length >= 2 ? reply(bulk(parts[1])) : reply("+PONG\r\n");
  if (command === "SET") {
    if (parts.length < 3) return reply("-ERR SET requires a key and value\r\n");
    database.set(parts[1], { value: parts[2] });
    return reply("+OK\r\n");
  }
  if (command === "GET") {
    if (parts.length < 2) return reply("-ERR GET requires a key\r\n");
    const record = purgeExpired(parts[1]);
    return record === undefined ? reply("$-1\r\n") : reply(bulk(record.value));
  }
  if (command === "DEL") {
    if (parts.length < 2) return reply("-ERR DEL requires a key\r\n");
    let removed = 0;
    for (const key of parts.slice(1)) {
      purgeExpired(key);
      if (database.delete(key) !== undefined) removed += 1;
    }
    return reply(integer(removed));
  }
  if (command === "EXISTS") {
    let found = 0;
    for (const key of parts.slice(1)) if (purgeExpired(key) !== undefined) found += 1;
    return reply(integer(found));
  }
  if (command === "INCR") {
    if (parts.length < 2) return reply("-ERR INCR requires a key\r\n");
    const record = purgeExpired(parts[1]);
    const value = record === undefined ? 0 : Number.parseInt(record.value, 10);
    if (!Number.isSafeInteger(value) || value < 0)
      return reply("-ERR value is not an integer\r\n");
    if (value === Number.MAX_SAFE_INTEGER)
      return reply("-ERR increment or decrement would overflow\r\n");
    const next = value + 1;
    database.set(parts[1], { value: String(next) });
    return reply(integer(next));
  }
  if (command === "EXPIRE") {
    if (parts.length < 3)
      return reply("-ERR EXPIRE requires a key and seconds\r\n");
    const seconds = Number.parseInt(parts[2], 10);
    if (!Number.isSafeInteger(seconds) || seconds < 0)
      return reply("-ERR value is not an integer\r\n");
    const record = purgeExpired(parts[1]);
    if (record === undefined) return reply(":0\r\n");
    record.expiresAt = Date.now() + seconds * 1_000;
    return reply(":1\r\n");
  }
  if (command === "TTL") {
    if (parts.length < 2) return reply("-ERR TTL requires a key\r\n");
    const record = purgeExpired(parts[1]);
    if (record === undefined) return reply(":-2\r\n");
    if (record.expiresAt === undefined) return reply(":-1\r\n");
    return reply(integer(Math.max(0, Math.floor((record.expiresAt - Date.now()) / 1_000))));
  }
  if (command === "COMMAND") return reply("*0\r\n");
  if (command === "QUIT") return reply("+OK\r\n", true);
  return reply("-ERR unknown command\r\n");
}

const server = createServer((socket) => {
  let input = Buffer.alloc(0);
  let closing = false;
  socket.on("data", (chunk) => {
    if (closing) return;
    input = input.length === 0 ? chunk : Buffer.concat([input, chunk]);
    try {
      while (input.length > 0) {
        const parsed = parseCommand(input);
        if (parsed === undefined) return;
        const result = execute(parsed.parts);
        closing = result.close;
        socket.write(result.payload, () => {
          if (result.close) socket.end();
        });
        input = input.subarray(parsed.consumed);
        if (closing) return;
      }
    } catch {
      socket.destroy();
    }
  });
  socket.on("error", () => {});
});

server.listen({ host, port });
