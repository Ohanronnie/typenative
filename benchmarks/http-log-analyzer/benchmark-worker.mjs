import { spawnSync } from "node:child_process";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { analyzeMany as analyzeJavaScript } from "./analyzer.mjs";

const [mode, fixturePath, iterationsText, samplesText, productPath] =
  process.argv.slice(2);
const iterations = Number.parseInt(iterationsText, 10);
const samples = Number.parseInt(samplesText, 10);
if (
  !["javascript", "addon", "native"].includes(mode) ||
  !Number.isInteger(iterations) ||
  iterations <= 0 ||
  !Number.isInteger(samples) ||
  samples < 5
) {
  throw new Error(
    "usage: benchmark-worker.mjs MODE FIXTURE ITERATIONS SAMPLES PRODUCT; samples must be at least 5",
  );
}

const fixture = readFileSync(fixturePath);
const malformed = Buffer.from(
  '10.0.0.1 - - [14/Aug/2026:00:00:00 +0100] "GET /broken HTTP/1.1" xx 10 2\n',
);

function median(values) {
  const ordered = [...values].sort((left, right) => left - right);
  const middle = Math.floor(ordered.length / 2);
  return ordered.length % 2 === 0
    ? (ordered[middle - 1] + ordered[middle]) / 2
    : ordered[middle];
}

function summarize(name, elapsedNanoseconds, checksums, extra = {}) {
  const elapsedMilliseconds = elapsedNanoseconds.map((value) => value / 1e6);
  const throughput = elapsedNanoseconds.map(
    (value) => (fixture.length * iterations) / (1024 * 1024) / (value / 1e9),
  );
  return {
    name,
    checksum: checksums[0],
    samples,
    iterations,
    elapsedMilliseconds: {
      median: median(elapsedMilliseconds),
      min: Math.min(...elapsedMilliseconds),
      max: Math.max(...elapsedMilliseconds),
    },
    throughputMiBPerSecond: {
      median: median(throughput),
      min: Math.min(...throughput),
      max: Math.max(...throughput),
    },
    ...extra,
  };
}

function validateChecksums(checksums) {
  if (checksums.some((value) => value === "0" || value !== checksums[0])) {
    throw new Error(`${mode} returned inconsistent checksums: ${checksums}`);
  }
}

if (mode === "javascript" || mode === "addon") {
  const analyze =
    mode === "javascript"
      ? analyzeJavaScript
      : createRequire(import.meta.url)(productPath).analyzeMany;
  if (String(analyze(malformed, 1)) !== "0") {
    throw new Error(`${mode} accepted malformed input`);
  }
  analyze(fixture, Math.max(1, Math.floor(iterations / 2)));
  const elapsed = [];
  const checksums = [];
  for (let sample = 0; sample < samples; sample += 1) {
    const started = process.hrtime.bigint();
    const checksum = analyze(fixture, iterations);
    elapsed.push(Number(process.hrtime.bigint() - started));
    checksums.push(String(checksum));
  }
  validateChecksums(checksums);
  const peakRssBytes = process.resourceUsage().maxRSS * 1024;
  process.stdout.write(
    `${JSON.stringify(
      summarize(
        mode === "javascript"
          ? "Node.js handwritten Buffer parser"
          : "TypeNative .node addon",
        elapsed,
        checksums,
        {
          peakRssBytes,
          peakRssMethod:
            "isolated Node process.resourceUsage().maxRSS after fixture load and measured samples",
        },
      ),
    )}\n`,
  );
} else {
  const malformedPath = `/tmp/typenative-http-log-malformed-${process.pid}.log`;
  writeFileSync(malformedPath, malformed);
  const malformedRun = spawnSync(productPath, [malformedPath, "1"], {
    encoding: "utf8",
  });
  rmSync(malformedPath, { force: true });
  if (malformedRun.status !== 7) {
    throw new Error(
      `native malformed-input check returned ${malformedRun.status}: ${malformedRun.stderr}`,
    );
  }

  const runProcess = (measuredIterations) => {
    const wallStarted = process.hrtime.bigint();
    const timeArguments = process.platform === "darwin" ? ["-l"] : ["-v"];
    const run = spawnSync(
      "/usr/bin/time",
      [...timeArguments, productPath, fixturePath, String(measuredIterations)],
      {
        encoding: "utf8",
        maxBuffer: 16 * 1024 * 1024,
      },
    );
    const wallNanoseconds = Number(process.hrtime.bigint() - wallStarted);
    if (run.status !== 0) {
      throw new Error(`native analyzer failed (${run.status}): ${run.stderr}`);
    }
    const rssMatch =
      process.platform === "darwin"
        ? run.stderr.match(/(\d+)\s+maximum resident set size/)
        : run.stderr.match(/Maximum resident set size \(kbytes\):\s+(\d+)/i);
    return {
      output: run.stdout.trim(),
      wallNanoseconds,
      peakRssBytes: rssMatch
        ? Number(rssMatch[1]) * (process.platform === "darwin" ? 1 : 1024)
        : null,
    };
  };

  const runNative = () => {
    const measured = runProcess(iterations);
    const [checksum, elapsedNanosecondsText] = measured.output.split(",");
    const coreNanoseconds = Number.parseInt(elapsedNanosecondsText, 10);
    if (!Number.isSafeInteger(coreNanoseconds) || coreNanoseconds <= 0) {
      throw new Error(
        `native analyzer returned invalid Instant duration: ${measured.output}`,
      );
    }
    return {
      checksum,
      coreNanoseconds,
      wallNanoseconds: measured.wallNanoseconds,
      peakRssBytes: measured.peakRssBytes,
    };
  };

  runNative();
  const runs = [];
  for (let sample = 0; sample < samples; sample += 1) runs.push(runNative());
  const checksums = runs.map((run) => run.checksum);
  validateChecksums(checksums);
  const result = summarize(
    "TypeNative optimized executable",
    runs.map((run) => run.coreNanoseconds),
    checksums,
    {
      peakRssBytes: Math.max(
        ...runs.map((run) => run.peakRssBytes ?? Number.NEGATIVE_INFINITY),
      ),
      peakRssMethod:
        process.platform === "darwin"
          ? "maximum resident set size from macOS /usr/bin/time -l, including loaded fixture"
          : "maximum resident set size from Linux /usr/bin/time -v, including loaded fixture",
      processWallMilliseconds: {
        median: median(runs.map((run) => run.wallNanoseconds / 1e6)),
        min: Math.min(...runs.map((run) => run.wallNanoseconds / 1e6)),
        max: Math.max(...runs.map((run) => run.wallNanoseconds / 1e6)),
      },
      processWallIncludes: "startup, fixture load, core parse, and shutdown",
      coreTimingMethod:
        "Instant.now() elapsed duration emitted by the TypeNative analyzer after fixture loading",
    },
  );
  if (!Number.isFinite(result.peakRssBytes)) result.peakRssBytes = null;
  process.stdout.write(`${JSON.stringify(result)}\n`);
}
