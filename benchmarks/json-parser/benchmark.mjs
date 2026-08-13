import { execFileSync } from "node:child_process";
import { readFileSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { arch, platform } from "node:os";
import { fileURLToPath } from "node:url";
import { parseMany as parseManyJavaScript } from "./parser.mjs";

const directory = fileURLToPath(new URL(".", import.meta.url));
const fixtureText = readFileSync(
  new URL("./fixture.json", import.meta.url),
  "utf8",
).trim();
const fixture = new TextEncoder().encode(fixtureText);
const iterations = Number.parseInt(process.env.BENCH_ITERATIONS ?? "50000", 10);
const samples = Number.parseInt(process.env.BENCH_SAMPLES ?? "10", 10);
const nativePath = `${directory}build/json-parser`;
const addonPath = `${directory}build/json-parser.node`;
const require = createRequire(import.meta.url);
const addon = require(addonPath);

if (
  !Number.isInteger(iterations) ||
  iterations <= 0 ||
  iterations > 2_147_483_647
) {
  throw new Error("BENCH_ITERATIONS must be a positive 32-bit integer");
}
if (!Number.isInteger(samples) || samples <= 0) {
  throw new Error("BENCH_SAMPLES must be a positive integer");
}

const expectedChecksum = parseManyJavaScript(fixture, 1);
if (
  expectedChecksum === 0 ||
  String(addon.parseMany(fixture, 1)) !== String(expectedChecksum)
) {
  throw new Error(
    "TypeNative and JavaScript parsers disagree on the valid fixture",
  );
}
for (const invalid of ["{", "[1,]", '{"key":01}', '"unterminated']) {
  const bytes = new TextEncoder().encode(invalid);
  if (parseManyJavaScript(bytes, 1) !== 0 || addon.parseMany(bytes, 1) !== 0n) {
    throw new Error(`a parser accepted invalid JSON: ${invalid}`);
  }
}
JSON.parse(fixtureText);

function elapsedNanoseconds(run) {
  const started = process.hrtime.bigint();
  const checksum = run();
  return { nanoseconds: Number(process.hrtime.bigint() - started), checksum };
}

function summarize(name, values, checksum) {
  const ordered = [...values].sort((left, right) => left - right);
  const middle = Math.floor(ordered.length / 2);
  const median =
    ordered.length % 2 === 0
      ? (ordered[middle - 1] + ordered[middle]) / 2
      : ordered[middle];
  const bytes = fixture.length * iterations;
  return {
    name,
    medianMilliseconds: median / 1e6,
    minMilliseconds: ordered[0] / 1e6,
    maxMilliseconds: ordered.at(-1) / 1e6,
    throughputMiBPerSecond: bytes / (1024 * 1024) / (median / 1e9),
    checksum: String(checksum),
  };
}

for (let index = 0; index < 3; index += 1) {
  parseManyJavaScript(fixture, Math.max(1, Math.floor(iterations / 10)));
  addon.parseMany(fixture, Math.max(1, Math.floor(iterations / 10)));
  for (
    let inner = 0;
    inner < Math.max(1, Math.floor(iterations / 10));
    inner += 1
  ) {
    JSON.parse(fixtureText);
  }
}

const javascriptTimes = [];
let javascriptChecksum = 0;
for (let sample = 0; sample < samples; sample += 1) {
  const result = elapsedNanoseconds(() =>
    parseManyJavaScript(fixture, iterations),
  );
  javascriptTimes.push(result.nanoseconds);
  javascriptChecksum = result.checksum;
}

const addonTimes = [];
let addonChecksum = 0n;
for (let sample = 0; sample < samples; sample += 1) {
  const result = elapsedNanoseconds(() => addon.parseMany(fixture, iterations));
  addonTimes.push(result.nanoseconds);
  addonChecksum = result.checksum;
}

const builtinTimes = [];
for (let sample = 0; sample < samples; sample += 1) {
  const result = elapsedNanoseconds(() => {
    let parsed;
    for (let iteration = 0; iteration < iterations; iteration += 1)
      parsed = JSON.parse(fixtureText);
    return parsed.routes.length;
  });
  builtinTimes.push(result.nanoseconds);
}

const nativeTimes = [];
for (let index = 0; index < 3; index += 1) {
  execFileSync(
    nativePath,
    [fixtureText, String(Math.max(1, Math.floor(iterations / 10)))],
    {
      stdio: "ignore",
    },
  );
}
for (let sample = 0; sample < samples; sample += 1) {
  const result = elapsedNanoseconds(() =>
    execFileSync(nativePath, [fixtureText, String(iterations)], {
      stdio: "ignore",
    }),
  );
  nativeTimes.push(result.nanoseconds);
}

if (String(javascriptChecksum) !== String(addonChecksum)) {
  throw new Error(
    `checksum mismatch: JavaScript=${javascriptChecksum} addon=${addonChecksum}`,
  );
}

const results = {
  generatedAt: new Date().toISOString(),
  environment: {
    platform: platform(),
    architecture: arch(),
    node: process.version,
    fixtureBytes: fixture.length,
    iterations,
    samples,
  },
  results: [
    summarize(
      "TypeNative executable (process wall)",
      nativeTimes,
      "validated-by-exit-status",
    ),
    summarize("TypeNative .node addon", addonTimes, addonChecksum),
    summarize(
      "Node.js handwritten parser",
      javascriptTimes,
      javascriptChecksum,
    ),
    summarize("Node.js JSON.parse", builtinTimes, "not-comparable"),
  ],
};

writeFileSync(
  new URL("./results.json", import.meta.url),
  `${JSON.stringify(results, null, 2)}\n`,
);
console.table(results.results);
