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
const samples = Number.parseInt(process.env.BENCH_SAMPLES ?? "9", 10);
const warmups = Number.parseInt(process.env.BENCH_WARMUPS ?? "2", 10);
const shuffleSeed = Number.parseInt(
  process.env.BENCH_SHUFFLE_SEED ?? "305419896",
  10,
);
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
if (!Number.isInteger(warmups) || warmups !== 2) {
  throw new Error("BENCH_WARMUPS must be exactly 2");
}
if (
  !Number.isInteger(shuffleSeed) ||
  shuffleSeed < 0 ||
  shuffleSeed > 0xffffffff
) {
  throw new Error("BENCH_SHUFFLE_SEED must be a uint32");
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

function shuffledPlan(names, repetitions, seed) {
  const plan = [];
  for (let repetition = 0; repetition < repetitions; repetition += 1) {
    for (const name of names) plan.push(name);
  }
  let state = seed >>> 0;
  for (let index = plan.length - 1; index > 0; index -= 1) {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
    const swapIndex = state % (index + 1);
    [plan[index], plan[swapIndex]] = [plan[swapIndex], plan[index]];
  }
  return plan;
}

const runners = {
  native: () =>
    elapsedNanoseconds(() =>
      execFileSync(nativePath, [fixtureText, String(iterations)], {
        stdio: "ignore",
      }),
    ),
  addon: () => elapsedNanoseconds(() => addon.parseMany(fixture, iterations)),
  javascript: () =>
    elapsedNanoseconds(() => parseManyJavaScript(fixture, iterations)),
  builtin: () =>
    elapsedNanoseconds(() => {
      let parsed;
      for (let iteration = 0; iteration < iterations; iteration += 1)
        parsed = JSON.parse(fixtureText);
      return parsed.routes.length;
    }),
};
const names = Object.keys(runners);
const warmupPlan = shuffledPlan(names, warmups, shuffleSeed ^ 0xa5a5a5a5);
for (const name of warmupPlan) runners[name]();

const measuredPlan = shuffledPlan(names, samples, shuffleSeed);
const measured = Object.fromEntries(
  names.map((name) => [name, { times: [], checksum: undefined }]),
);
for (const name of measuredPlan) {
  const result = runners[name]();
  measured[name].times.push(result.nanoseconds);
  measured[name].checksum = result.checksum;
}

if (String(measured.javascript.checksum) !== String(measured.addon.checksum)) {
  throw new Error(
    `checksum mismatch: JavaScript=${measured.javascript.checksum} addon=${measured.addon.checksum}`,
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
    warmups,
    shuffleSeed,
  },
  methodology: {
    warmupPlan,
    measuredPlan,
    executionOrder: "deterministic Fisher-Yates shuffle per phase",
  },
  results: [
    summarize(
      "TypeNative executable (process wall)",
      measured.native.times,
      "validated-by-exit-status",
    ),
    summarize(
      "TypeNative .node addon",
      measured.addon.times,
      measured.addon.checksum,
    ),
    summarize(
      "Node.js handwritten parser",
      measured.javascript.times,
      measured.javascript.checksum,
    ),
    summarize("Node.js JSON.parse", measured.builtin.times, "not-comparable"),
  ],
};

writeFileSync(
  process.env.BENCH_RESULTS ??
    fileURLToPath(new URL("./results.json", import.meta.url)),
  `${JSON.stringify(results, null, 2)}\n`,
);
console.table(results.results);
