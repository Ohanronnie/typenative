import { execFileSync } from "node:child_process";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import { arch, cpus, platform, release } from "node:os";
import { fileURLToPath } from "node:url";

const directory = fileURLToPath(new URL(".", import.meta.url));
const fixturePath = process.env.BENCH_FIXTURE;
const iterations = Number.parseInt(process.env.BENCH_ITERATIONS ?? "1", 10);
const samples = Number.parseInt(process.env.BENCH_SAMPLES ?? "5", 10);
if (!fixturePath || !Number.isInteger(iterations) || iterations <= 0) {
  throw new Error("BENCH_FIXTURE and a positive BENCH_ITERATIONS are required");
}
if (!Number.isInteger(samples) || samples < 5) {
  throw new Error("BENCH_SAMPLES must be at least 5");
}

const worker = `${directory}benchmark-worker.mjs`;
const runWorker = (mode, product = "-") =>
  JSON.parse(
    execFileSync(
      process.execPath,
      [worker, mode, fixturePath, String(iterations), String(samples), product],
      { encoding: "utf8", maxBuffer: 16 * 1024 * 1024 },
    ),
  );

const modes = [
  runWorker("native", `${directory}build/http-log-analyzer`),
  runWorker("addon", `${directory}build/http-log-analyzer.node`),
  runWorker("javascript"),
];
if (modes.some((mode) => mode.checksum !== modes[0].checksum)) {
  throw new Error(
    `checksum mismatch: ${modes.map((mode) => `${mode.name}=${mode.checksum}`).join(", ")}`,
  );
}

function parseTiming(name) {
  const text = readFileSync(`${directory}build/${name}.time`, "utf8");
  const value = (key) =>
    Number.parseFloat(
      text.match(new RegExp(`^${key}\\s+([0-9.]+)$`, "m"))?.[1],
    );
  return {
    realSeconds: value("real"),
    userSeconds: value("user"),
    sysSeconds: value("sys"),
  };
}

const commandOutput = (command, arguments_) => {
  try {
    return execFileSync(command, arguments_, { encoding: "utf8" }).trim();
  } catch {
    return "unavailable";
  }
};
const llvmConfigCandidates = [
  process.env.LLVM_CONFIG,
  "/opt/homebrew/opt/llvm/bin/llvm-config",
  "/usr/local/opt/llvm/bin/llvm-config",
  "llvm-config-22",
  "llvm-config",
].filter(Boolean);
const llvmVersion =
  llvmConfigCandidates
    .map((command) => commandOutput(command, ["--version"]))
    .find((value) => value !== "unavailable") ?? "unavailable";
const workerCommand = (mode, product = "-") =>
  [
    process.execPath,
    worker,
    mode,
    fixturePath,
    iterations,
    samples,
    product,
  ].join(" ");
const results = {
  generatedAt: new Date().toISOString(),
  environment: {
    platform: platform(),
    release: release(),
    architecture: arch(),
    cpu: cpus()[0]?.model ?? "unknown",
    logicalCpus: cpus().length,
    node: process.version,
    typenative: commandOutput(process.env.TN_BIN, ["--version"]),
    llvm: llvmVersion,
    fixturePath,
    fixtureBytes: statSync(fixturePath).size,
    iterations,
    samples,
  },
  compilerTimings: {
    check: parseTiming("check"),
    debugExecutable: parseTiming("build-debug"),
    optimizedExecutable: parseTiming("build-optimized"),
    optimizedNodeAddon: parseTiming("build-addon"),
  },
  methodology: {
    fixtureGenerationExcluded: true,
    fixtureLoadingExcludedFromCoreTiming: true,
    executionOrder: ["native", "addon", "javascript"],
    warmup: "one unmeasured parse per isolated implementation process",
    caveat:
      "Results describe this deterministic byte-scanning and aggregation workload only.",
  },
  commands: {
    orchestration:
      process.env.BENCH_COMMAND ??
      `node ${directory}run.sh BENCH_FIXTURE=${fixturePath}`,
    fixtureGeneration:
      process.env.BENCH_FIXTURE_COMMAND ??
      `node ${directory}generate-fixture.mjs ${fixturePath} unknown`,
    workerTemplate: `${process.execPath} ${worker} MODE FIXTURE ITERATIONS SAMPLES PRODUCT`,
    workerRuns: {
      native: workerCommand("native", `${directory}build/http-log-analyzer`),
      addon: workerCommand("addon", `${directory}build/http-log-analyzer.node`),
      javascript: workerCommand("javascript"),
    },
  },
  checksum: modes[0].checksum,
  results: modes,
};
writeFileSync(
  `${directory}results.json`,
  `${JSON.stringify(results, null, 2)}\n`,
);

console.table(
  modes.map((mode) => ({
    implementation: mode.name,
    medianMiBPerSecond: mode.throughputMiBPerSecond.median.toFixed(2),
    minMiBPerSecond: mode.throughputMiBPerSecond.min.toFixed(2),
    maxMiBPerSecond: mode.throughputMiBPerSecond.max.toFixed(2),
    medianMilliseconds: mode.elapsedMilliseconds.median.toFixed(2),
    peakRssMiB:
      mode.peakRssBytes === null
        ? "unavailable"
        : (mode.peakRssBytes / (1024 * 1024)).toFixed(2),
  })),
);
console.log(`checksum: ${results.checksum}`);
console.log(`results: ${directory}results.json`);
