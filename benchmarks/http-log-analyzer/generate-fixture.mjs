import { closeSync, openSync, writeSync } from "node:fs";

const output = process.argv[2];
const targetMiB = Number.parseFloat(process.argv[3] ?? "100");
if (!output || !Number.isFinite(targetMiB) || targetMiB <= 0) {
  throw new Error("usage: node generate-fixture.mjs OUTPUT TARGET_MIB");
}

const routes = [
  "/",
  "/health",
  "/api/users",
  "/api/users/42",
  "/api/orders",
  "/api/orders/20260814",
  "/api/search?q=typenative",
  "/assets/app.js",
  "/assets/app.css",
  "/images/avatar.webp",
  "/docs/compiler",
  "/docs/runtime",
];
const methods = ["GET", "GET", "GET", "POST", "PUT", "DELETE"];
const statuses = [200, 200, 200, 201, 204, 301, 304, 400, 404, 429, 500, 503];
const targetBytes = Math.floor(targetMiB * 1024 * 1024);
const file = openSync(output, "w");
let written = 0;
let record = 0;
let chunk = "";

while (written < targetBytes) {
  const route = routes[(record * 7 + Math.floor(record / 11)) % routes.length];
  const method = methods[(record * 5 + 3) % methods.length];
  const status =
    statuses[(record * 13 + Math.floor(record / 17)) % statuses.length];
  const octet3 = Math.floor(record / 251) % 251;
  const octet4 = (record % 251) + 1;
  const hour = String(Math.floor(record / 3600) % 24).padStart(2, "0");
  const minute = String(Math.floor(record / 60) % 60).padStart(2, "0");
  const second = String(record % 60).padStart(2, "0");
  const bytes = (record * 7919) % 1_000_000;
  const duration = (record * 97) % 5_000;
  const line = `10.24.${octet3}.${octet4} - - [14/Aug/2026:${hour}:${minute}:${second} +0100] "${method} ${route} HTTP/1.1" ${status} ${bytes} ${duration}\n`;
  if (
    written + Buffer.byteLength(chunk) + Buffer.byteLength(line) >
    targetBytes
  )
    break;
  chunk += line;
  record += 1;
  if (chunk.length >= 1024 * 1024) {
    const bytesWritten = writeSync(file, chunk);
    written += bytesWritten;
    chunk = "";
  }
}
if (chunk.length > 0) written += writeSync(file, chunk);
closeSync(file);
process.stdout.write(
  `${JSON.stringify({ output, bytes: written, records: record })}\n`,
);
