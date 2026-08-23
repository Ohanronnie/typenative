import { execFileSync, spawn } from "node:child_process";
import { createHash } from "node:crypto";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import { connect } from "node:net";
import { arch, platform } from "node:os";
import { fileURLToPath } from "node:url";

const directory = fileURLToPath(new URL(".", import.meta.url));
const portBase = 10_000 + ((process.pid * 17 + Date.now()) % 20_000);
const addonPortBase = portBase;
const nativePortBase = portBase + 100;
const handwrittenPortBase = portBase + 200;
const rustPortBase = portBase + 300;
const sampleCount = positiveInteger("BENCH_SAMPLES", 9);
const warmupCount = positiveInteger("BENCH_WARMUPS", 2);
const pingCount = positiveInteger("BENCH_PING_COUNT", 100_000);
const nonPipelinedPingCount = positiveInteger(
  "BENCH_NONPIPE_PING_COUNT",
  10_000,
);
const operationCount = positiveInteger("BENCH_OPERATION_COUNT", 10_000);
const concurrentClients = positiveInteger("BENCH_CONCURRENT_CLIENTS", 8);
const largeValueBytes = positiveInteger("BENCH_LARGE_VALUE", 12_000);
const randomSeed = 0x1234_5678;
const shuffleSeed = positiveInteger("BENCH_SHUFFLE_SEED", 0x1357_9bdf);

function positiveInteger(name, fallback) {
  const value = Number.parseInt(process.env[name] ?? String(fallback), 10);
  if (!Number.isInteger(value) || value <= 0)
    throw new Error(`${name} must be a positive integer`);
  return value;
}

function shuffledPlan(implementationCount, repetitions, seed) {
  const plan = [];
  for (let repetition = 0; repetition < repetitions; repetition += 1) {
    for (
      let implementationIndex = 0;
      implementationIndex < implementationCount;
      implementationIndex += 1
    ) {
      plan.push({ implementationIndex, repetition });
    }
  }
  let state = seed >>> 0;
  for (let index = plan.length - 1; index > 0; index -= 1) {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
    const swapIndex = state % (index + 1);
    [plan[index], plan[swapIndex]] = [plan[swapIndex], plan[index]];
  }
  return plan;
}

if (warmupCount !== 2) throw new Error("BENCH_WARMUPS must be exactly 2");

function argumentValue(name) {
  const index = process.argv.indexOf(name);
  return index < 0 ? undefined : process.argv[index + 1];
}

const compilerCommit =
  argumentValue("--compiler-commit") ??
  process.env.COMPILER_COMMIT ??
  "unknown";

function frame(...parts) {
  const chunks = [Buffer.from(`*${parts.length}\r\n`)];
  for (const part of parts) {
    const payload = Buffer.from(String(part));
    chunks.push(
      Buffer.from(`$${payload.length}\r\n`),
      payload,
      Buffer.from("\r\n"),
    );
  }
  return Buffer.concat(chunks);
}

class RespClient {
  constructor(socket) {
    this.socket = socket;
    this.input = Buffer.alloc(0);
    this.waiters = [];
    socket.on("data", (chunk) => {
      this.input =
        this.input.length === 0 ? chunk : Buffer.concat([this.input, chunk]);
      this.drain();
    });
    socket.on("error", (error) => {
      while (this.waiters.length > 0) this.waiters.shift().reject(error);
    });
  }

  parseResponse() {
    const lineEnd = this.input.indexOf("\r\n", 0, "ascii");
    if (lineEnd < 0) return undefined;
    const prefix = this.input[0];
    if (prefix === 43 || prefix === 45 || prefix === 58) {
      const response = this.input.subarray(0, lineEnd + 2);
      this.input = this.input.subarray(lineEnd + 2);
      return response;
    }
    if (prefix !== 36) throw new Error("unexpected RESP response");
    const length = Number.parseInt(
      this.input.toString("ascii", 1, lineEnd),
      10,
    );
    if (length < 0) {
      const response = this.input.subarray(0, lineEnd + 2);
      this.input = this.input.subarray(lineEnd + 2);
      return response;
    }
    const end = lineEnd + 2 + length + 2;
    if (this.input.length < end) return undefined;
    const response = this.input.subarray(0, end);
    this.input = this.input.subarray(end);
    return response;
  }

  drain() {
    while (this.waiters.length > 0) {
      let response;
      try {
        response = this.parseResponse();
      } catch (error) {
        this.waiters.shift().reject(error);
        continue;
      }
      if (response === undefined) return;
      this.waiters.shift().resolve(response);
    }
  }

  response() {
    return new Promise((resolve, reject) => {
      this.waiters.push({ resolve, reject });
      this.drain();
    });
  }

  write(payload) {
    return new Promise((resolve, reject) => {
      this.socket.write(payload, (error) =>
        error ? reject(error) : resolve(),
      );
    });
  }

  close() {
    this.socket.destroy();
  }
}

function openClient(serverPort) {
  return new Promise((resolve, reject) => {
    const socket = connect({ host: "127.0.0.1", port: serverPort });
    socket.once("connect", () => resolve(new RespClient(socket)));
    socket.once("error", reject);
  });
}

async function command(client, ...parts) {
  await client.write(frame(...parts));
  return client.response();
}

function equal(actual, expected, label) {
  if (!actual.equals(Buffer.from(expected)))
    throw new Error(
      `${label}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(String(actual))}`,
    );
}

async function waitUntilReady(child, implementationName, stderr, serverPort) {
  const started = process.hrtime.bigint();
  for (let attempt = 0; attempt < 240; attempt += 1) {
    if (child.exitCode !== null)
      throw new Error(
        `${implementationName} exited during startup with ${child.exitCode}: ${stderr()}`,
      );
    try {
      const client = await openClient(serverPort);
      try {
        const response = await command(client, "PING");
        if (String(response) === "+PONG\r\n")
          return Number(process.hrtime.bigint() - started) / 1e6;
      } finally {
        client.close();
      }
    } catch {
      await new Promise((resolve) => setTimeout(resolve, 25));
    }
  }
  throw new Error(
    `${implementationName} did not become ready on ${serverPort}`,
  );
}

function rssKiB(pid) {
  const output = execFileSync("ps", ["-o", "rss=", "-p", String(pid)], {
    encoding: "utf8",
  }).trim();
  const value = Number.parseInt(output, 10);
  if (!Number.isInteger(value))
    throw new Error(`could not read RSS for ${pid}`);
  return value;
}

function processMetrics(pid) {
  return JSON.parse(
    execFileSync(
      "python3",
      [`${directory}../../scripts/process-metrics.py`, String(pid)],
      { encoding: "utf8" },
    ),
  );
}

function metricDelta(after, before, name) {
  const value = after[name] - before[name];
  if (!Number.isSafeInteger(value) || value < 0)
    throw new Error(`invalid ${name} process metric delta`);
  return value;
}

function buildTiming(name) {
  const timing = readFileSync(`${directory}build/${name}.time`, "utf8");
  const match = /^real ([0-9.]+)$/m.exec(timing);
  if (match === null) throw new Error(`missing real time in ${name}.time`);
  const realSeconds = Number.parseFloat(match[1]);
  if (realSeconds > 175)
    throw new Error(`${name} compiler invocation exceeded 175 seconds`);
  const phaseText = readFileSync(`${directory}build/${name}.phases`, "utf8");
  const phaseMicroseconds = {};
  for (const phase of phaseText.matchAll(
    /^tn-timing phase=([^ ]+) micros=([0-9]+)$/gm,
  ))
    phaseMicroseconds[phase[1]] = Number.parseInt(phase[2], 10);
  for (const required of [
    "module-check",
    "ownership",
    "mir-drop",
    "monomorphization",
    "llvm-link",
  ])
    if (phaseMicroseconds[required] === undefined)
      throw new Error(`${name} is missing compiler phase ${required}`);
  return { realSeconds, phaseMicroseconds };
}

async function validate(client, serverPort) {
  equal(await command(client, "PING"), "+PONG\r\n", "PING");
  equal(await command(client, "SET", "user", "ronnie"), "+OK\r\n", "SET");
  equal(await command(client, "GET", "user"), "$6\r\nronnie\r\n", "GET");
  equal(await command(client, "DEL", "user"), ":1\r\n", "DEL");
  equal(await command(client, "GET", "user"), "$-1\r\n", "missing GET");
  equal(
    await command(client, "UNKNOWN"),
    "-ERR unknown command\r\n",
    "unknown",
  );

  await client.write(
    Buffer.concat([frame("SET", "pipeline", "ok"), frame("GET", "pipeline")]),
  );
  equal(await client.response(), "+OK\r\n", "pipeline SET");
  equal(await client.response(), "$2\r\nok\r\n", "pipeline GET");

  const fragmented = frame("PING");
  for (const byte of fragmented) await client.write(Buffer.from([byte]));
  equal(await client.response(), "+PONG\r\n", "fragmented PING");

  const largeValue = "x".repeat(largeValueBytes);
  equal(
    await command(client, "SET", "large", largeValue),
    "+OK\r\n",
    "large SET",
  );
  equal(
    await command(client, "GET", "large"),
    `$${largeValueBytes}\r\n${largeValue}\r\n`,
    "large GET",
  );
  equal(await command(client, "DEL", "large"), ":1\r\n", "large DEL");

  const clients = await Promise.all(
    Array.from({ length: concurrentClients }, () => openClient(serverPort)),
  );
  try {
    await Promise.all(
      clients.map(async (concurrentClient, index) => {
        const key = `concurrent-${index}`;
        const value = `value-${index}`;
        equal(
          await command(concurrentClient, "SET", key, value),
          "+OK\r\n",
          `concurrent SET ${index}`,
        );
        equal(
          await command(concurrentClient, "GET", key),
          `$${Buffer.byteLength(value)}\r\n${value}\r\n`,
          `concurrent GET ${index}`,
        );
        equal(
          await command(concurrentClient, "DEL", key),
          ":1\r\n",
          `concurrent DEL ${index}`,
        );
      }),
    );
  } finally {
    for (const concurrentClient of clients) concurrentClient.close();
  }
}

async function validateMalformedFrame(serverPort) {
  const socket = await new Promise((resolve, reject) => {
    const candidate = connect({ host: "127.0.0.1", port: serverPort });
    candidate.once("connect", () => resolve(candidate));
    candidate.once("error", reject);
  });
  await new Promise((resolve, reject) => {
    let settled = false;
    const finish = (error) => {
      if (settled) return;
      settled = true;
      clearTimeout(timer);
      error ? reject(error) : resolve();
    };
    const timer = setTimeout(
      () => finish(new Error("malformed RESP connection was not closed")),
      1_000,
    );
    socket.once("close", () => finish());
    socket.once("error", () => {});
    socket.write("*1\r\n$4\r\nPING\rX", (error) => {
      if (error) finish(error);
    });
  });
}

async function correctnessChecksum(client) {
  const checksum = createHash("sha256");
  for (let index = 0; index < 128; index += 1) {
    const key = `checksum-${index}`;
    const value = `value-${(index * 17) % 251}`;
    for (const response of [
      await command(client, "SET", key, value),
      await command(client, "GET", key),
      await command(client, "DEL", key),
      await command(client, "PING"),
    ])
      checksum.update(response);
  }
  return checksum.digest("hex");
}

function randomGenerator() {
  let state = randomSeed;
  return (limit) => {
    state = (Math.imul(state, 1_664_525) + 1_013_904_223) >>> 0;
    return state % limit;
  };
}

async function measurePipelinedPing(client) {
  const ping = frame("PING");
  const batchSize = 1_000;
  let completed = 0;
  const started = process.hrtime.bigint();
  while (completed < pingCount) {
    const count = Math.min(batchSize, pingCount - completed);
    await client.write(
      Buffer.concat(Array.from({ length: count }, () => ping)),
    );
    for (let index = 0; index < count; index += 1)
      equal(await client.response(), "+PONG\r\n", "pipelined PING");
    completed += count;
  }
  const seconds = Number(process.hrtime.bigint() - started) / 1e9;
  return { seconds, perSecond: Math.round(pingCount / seconds) };
}

async function measureNonPipelinedPing(client) {
  const started = process.hrtime.bigint();
  for (let index = 0; index < nonPipelinedPingCount; index += 1)
    equal(await command(client, "PING"), "+PONG\r\n", "non-pipelined PING");
  const seconds = Number(process.hrtime.bigint() - started) / 1e9;
  return {
    seconds,
    perSecond: Math.round(nonPipelinedPingCount / seconds),
  };
}

async function measureRandomSetGet(client) {
  const keys = Array.from({ length: 256 }, (_, index) => `benchmark-${index}`);
  const random = randomGenerator();
  let started = process.hrtime.bigint();
  for (let index = 0; index < operationCount; index += 1) {
    const key = keys[random(keys.length)];
    const value = `value-${random(1_024)}`;
    equal(await command(client, "SET", key, value), "+OK\r\n", "random SET");
  }
  const setSeconds = Number(process.hrtime.bigint() - started) / 1e9;

  started = process.hrtime.bigint();
  for (let index = 0; index < operationCount; index += 1) {
    const key = keys[random(keys.length)];
    const response = await command(client, "GET", key);
    if (response[0] !== 36)
      throw new Error("random GET did not return a bulk value");
  }
  const getSeconds = Number(process.hrtime.bigint() - started) / 1e9;
  return {
    setSeconds,
    setPerSecond: Math.round(operationCount / setSeconds),
    getSeconds,
    getPerSecond: Math.round(operationCount / getSeconds),
  };
}

async function benchmark(client, pid) {
  for (let index = 0; index < 100; index += 1)
    equal(await command(client, "PING"), "+PONG\r\n", "PING warmup");

  const initialRssKiB = rssKiB(pid);
  const initialProcessMetrics = processMetrics(pid);
  const pipelinedPing = await measurePipelinedPing(client);
  const nonPipelinedPing = await measureNonPipelinedPing(client);
  const randomSetGet = await measureRandomSetGet(client);
  const finalRssKiB = rssKiB(pid);
  const finalProcessMetrics = processMetrics(pid);
  return {
    pipelinedPingSeconds: pipelinedPing.seconds,
    pipelinedPingPerSecond: pipelinedPing.perSecond,
    nonPipelinedPingSeconds: nonPipelinedPing.seconds,
    nonPipelinedPingPerSecond: nonPipelinedPing.perSecond,
    nonPipelinedPingLatencyMicroseconds:
      (nonPipelinedPing.seconds * 1e6) / nonPipelinedPingCount,
    randomSetSeconds: randomSetGet.setSeconds,
    randomSetPerSecond: randomSetGet.setPerSecond,
    randomGetSeconds: randomSetGet.getSeconds,
    randomGetPerSecond: randomSetGet.getPerSecond,
    initialRssKiB,
    finalRssKiB,
    rssGrowthKiB: finalRssKiB - initialRssKiB,
    cpuUserNanoseconds: metricDelta(
      finalProcessMetrics,
      initialProcessMetrics,
      "total_user_nanoseconds",
    ),
    cpuSystemNanoseconds: metricDelta(
      finalProcessMetrics,
      initialProcessMetrics,
      "total_system_nanoseconds",
    ),
    machSystemCalls: metricDelta(
      finalProcessMetrics,
      initialProcessMetrics,
      "mach_syscalls",
    ),
    unixSystemCalls: metricDelta(
      finalProcessMetrics,
      initialProcessMetrics,
      "unix_syscalls",
    ),
    contextSwitches: metricDelta(
      finalProcessMetrics,
      initialProcessMetrics,
      "context_switches",
    ),
  };
}

async function stop(child) {
  if (child.exitCode !== null) return;
  child.kill("SIGTERM");
  await Promise.race([
    new Promise((resolve) => child.once("exit", resolve)),
    new Promise((resolve) => setTimeout(resolve, 2_000)),
  ]);
  if (child.exitCode === null) child.kill("SIGKILL");
}

function median(values) {
  const sorted = [...values].sort((left, right) => left - right);
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 === 0
    ? (sorted[middle - 1] + sorted[middle]) / 2
    : sorted[middle];
}

function standardDeviation(values) {
  if (values.length < 2) return 0;
  const mean = values.reduce((sum, value) => sum + value, 0) / values.length;
  return Math.sqrt(
    values.reduce((sum, value) => sum + (value - mean) ** 2, 0) /
      (values.length - 1),
  );
}

function tCritical95(count) {
  const values = {
    2: 12.706,
    3: 4.303,
    4: 3.182,
    5: 2.776,
    6: 2.571,
    7: 2.447,
    8: 2.365,
    9: 2.306,
    10: 2.262,
  };
  return values[count] ?? 1.96;
}

function statistics(values) {
  const mean = values.reduce((sum, value) => sum + value, 0) / values.length;
  const deviation = standardDeviation(values);
  const margin =
    values.length < 2
      ? 0
      : (tCritical95(values.length) * deviation) / Math.sqrt(values.length);
  const center = median(values);
  return {
    min: Math.min(...values),
    median: center,
    max: Math.max(...values),
    mean,
    standardDeviation: deviation,
    medianAbsoluteDeviation: median(
      values.map((value) => Math.abs(value - center)),
    ),
    confidenceInterval95: [mean - margin, mean + margin],
  };
}

function summarize(samples) {
  const metricNames = [
    "startupMilliseconds",
    "pipelinedPingSeconds",
    "pipelinedPingPerSecond",
    "nonPipelinedPingSeconds",
    "nonPipelinedPingPerSecond",
    "nonPipelinedPingLatencyMicroseconds",
    "randomSetSeconds",
    "randomSetPerSecond",
    "randomGetSeconds",
    "randomGetPerSecond",
    "initialRssKiB",
    "finalRssKiB",
    "rssGrowthKiB",
    "cpuUserNanoseconds",
    "cpuSystemNanoseconds",
    "machSystemCalls",
    "unixSystemCalls",
    "contextSwitches",
  ];
  return Object.fromEntries(
    metricNames.map((name) => [
      name,
      statistics(samples.map((sample) => sample[name])),
    ]),
  );
}

function pairedComparison(leftSamples, rightSamples, metric) {
  const rightBySample = new Map(
    rightSamples.map((sample) => [sample.sample, sample[metric]]),
  );
  const differences = leftSamples.map(
    (sample) => sample[metric] - rightBySample.get(sample.sample),
  );
  const ratios = leftSamples.map(
    (sample) => sample[metric] / rightBySample.get(sample.sample),
  );
  const ratio = statistics(ratios);
  return {
    metric,
    direction: metric.includes("Latency")
      ? "left divided by right; lower favors left latency"
      : "left divided by right; higher favors left throughput",
    difference: statistics(differences),
    ratio,
    fivePercentMargin: {
      lower: 0.95,
      upper: 1.05,
      confidenceIntervalWithinMargin:
        ratio.confidenceInterval95[0] >= 0.95 &&
        ratio.confidenceInterval95[1] <= 1.05,
      confidenceIntervalAtLeastEquivalent:
        ratio.confidenceInterval95[0] >= 0.95,
    },
  };
}

const allImplementations = [
  {
    name: "TypeNative .node addon",
    command: process.execPath,
    arguments: [`${directory}addon-server.mjs`, `${directory}build/redis.node`],
    artifactBytes: statSync(`${directory}build/redis.node`).size,
    basePort: addonPortBase,
    portArgument: true,
  },
  {
    name: "TypeNative native executable",
    command: `${directory}build/redis-native`,
    arguments: [],
    artifactBytes: statSync(`${directory}build/redis-native`).size,
    basePort: nativePortBase,
  },
  {
    name: "Node.js handwritten",
    command: process.execPath,
    arguments: [`${directory}server.mjs`],
    artifactBytes: null,
    basePort: handwrittenPortBase,
  },
  {
    name: "Rust native executable",
    command: `${directory}build/redis-rust`,
    arguments: [],
    artifactBytes: statSync(`${directory}build/redis-rust`).size,
    basePort: rustPortBase,
  },
];

async function runSample(implementation, portOffset, sampleNumber) {
  const serverPort = implementation.basePort + portOffset;
  const childArguments = implementation.portArgument
    ? [...implementation.arguments, String(serverPort)]
    : implementation.arguments;
  const child = spawn(implementation.command, childArguments, {
    stdio: ["ignore", "pipe", "pipe"],
    env: {
      ...process.env,
      REDIS_NATIVE_PORT: String(serverPort),
      REDIS_PORT: String(serverPort),
      REDIS_RUST_PORT: String(serverPort),
    },
  });
  let stderr = "";
  let result;
  child.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  try {
    const startupMilliseconds = await waitUntilReady(
      child,
      implementation.name,
      () => stderr,
      serverPort,
    );
    const client = await openClient(serverPort);
    try {
      await validate(client, serverPort);
      await validateMalformedFrame(serverPort);
      const responseChecksumSha256 = await correctnessChecksum(client);
      result = {
        ...(sampleNumber === undefined ? {} : { sample: sampleNumber }),
        port: serverPort,
        startupMilliseconds,
        responseChecksumSha256,
        ...(await benchmark(client, child.pid)),
      };
    } finally {
      client.close();
    }
  } finally {
    await stop(child);
  }
  if (child.exitCode && child.signalCode === null)
    throw new Error(`${implementation.name} failed: ${stderr}`);
  return result;
}

const warmupPlan = shuffledPlan(
  allImplementations.length,
  warmupCount,
  shuffleSeed ^ 0xa5a5_a5a5,
);
for (const run of warmupPlan)
  await runSample(
    allImplementations[run.implementationIndex],
    sampleCount + warmupCount + run.repetition,
    undefined,
  );

const measuredPlan = shuffledPlan(
  allImplementations.length,
  sampleCount,
  shuffleSeed,
);
const measuredSamples = allImplementations.map(() => []);
for (const run of measuredPlan) {
  measuredSamples[run.implementationIndex].push(
    await runSample(
      allImplementations[run.implementationIndex],
      warmupCount + run.repetition,
      run.repetition + 1,
    ),
  );
}

const results = allImplementations.map((implementation, index) => ({
  name: implementation.name,
  artifactBytes: implementation.artifactBytes,
  samples: measuredSamples[index].sort(
    (left, right) => left.sample - right.sample,
  ),
  summary: summarize(measuredSamples[index]),
}));
const checksumSet = new Set(
  results.flatMap((implementation) =>
    implementation.samples.map((sample) => sample.responseChecksumSha256),
  ),
);
if (checksumSet.size !== 1)
  throw new Error(`response checksum mismatch: ${[...checksumSet].join(", ")}`);
const addonResult = results.find((result) => result.name.includes(".node"));
const nativeResult = results.find((result) => result.name.includes("native"));
const handwrittenResult = results.find((result) =>
  result.name.includes("handwritten"),
);
const rustResult = results.find((result) => result.name.startsWith("Rust"));

const report = {
  generatedAt: new Date().toISOString(),
  compilerCommit,
  environment: {
    platform: platform(),
    architecture: arch(),
    node: process.version,
  },
  workload: {
    samples: sampleCount,
    warmups: warmupCount,
    pipelinedPingCount: pingCount,
    nonPipelinedPingCount,
    randomizedSetCount: operationCount,
    randomizedGetCount: operationCount,
    concurrentClients,
    largeValueBytes,
    randomSeed,
    shuffleSeed,
    portBases: {
      addon: addonPortBase,
      native: nativePortBase,
      handwritten: handwrittenPortBase,
      rust: rustPortBase,
    },
  },
  methodology: {
    warmupPlan: warmupPlan.map(
      (run) => allImplementations[run.implementationIndex].name,
    ),
    measuredPlan: measuredPlan.map(
      (run) =>
        `${allImplementations[run.implementationIndex].name}#${run.repetition + 1}`,
    ),
    executionOrder: "deterministic Fisher-Yates shuffle per phase",
  },
  compilation: {
    incrementalDefinition:
      "unchanged-input rebuild with the existing output artifact",
    addon: {
      clean: buildTiming("build-addon-clean"),
      incremental: buildTiming("build-addon-incremental"),
    },
    native: {
      clean: buildTiming("build-native-clean"),
      incremental: buildTiming("build-native-incremental"),
    },
  },
  correctness: {
    responseChecksumSha256: [...checksumSet][0],
    checkedBeforeTiming: true,
  },
  comparisons: {
    nativeVersusHandwrittenPipelinedPing: pairedComparison(
      nativeResult.samples,
      handwrittenResult.samples,
      "pipelinedPingPerSecond",
    ),
    addonVersusHandwrittenPipelinedPing: pairedComparison(
      addonResult.samples,
      handwrittenResult.samples,
      "pipelinedPingPerSecond",
    ),
    nativeVersusRustPipelinedPing: pairedComparison(
      nativeResult.samples,
      rustResult.samples,
      "pipelinedPingPerSecond",
    ),
    addonVersusRustPipelinedPing: pairedComparison(
      addonResult.samples,
      rustResult.samples,
      "pipelinedPingPerSecond",
    ),
    nativeVersusRustNonPipelinedLatency: pairedComparison(
      nativeResult.samples,
      rustResult.samples,
      "nonPipelinedPingLatencyMicroseconds",
    ),
    nativeVersusRustSet: pairedComparison(
      nativeResult.samples,
      rustResult.samples,
      "randomSetPerSecond",
    ),
    nativeVersusRustGet: pairedComparison(
      nativeResult.samples,
      rustResult.samples,
      "randomGetPerSecond",
    ),
  },
  implementations: results,
};

writeFileSync(
  process.env.BENCH_RESULTS ?? `${directory}results.json`,
  `${JSON.stringify(report, null, 2)}\n`,
);
for (const result of results)
  console.table(
    result.samples.map((sample) => ({
      implementation: result.name,
      sample: sample.sample,
      startupMs: sample.startupMilliseconds,
      pipelinedPingPerSecond: sample.pipelinedPingPerSecond,
      nonPipelinedPingPerSecond: sample.nonPipelinedPingPerSecond,
      randomSetPerSecond: sample.randomSetPerSecond,
      randomGetPerSecond: sample.randomGetPerSecond,
      rssGrowthKiB: sample.rssGrowthKiB,
    })),
  );
console.log("redis-comparison=pass");
