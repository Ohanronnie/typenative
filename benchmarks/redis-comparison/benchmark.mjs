import { execFileSync, spawn } from "node:child_process";
import { readFileSync, statSync, writeFileSync } from "node:fs";
import { connect } from "node:net";
import { arch, platform } from "node:os";
import { fileURLToPath } from "node:url";

const directory = fileURLToPath(new URL(".", import.meta.url));
const portBase = 10_000 + ((process.pid * 17 + Date.now()) % 20_000);
const addonPortBase = portBase;
const nativePortBase = portBase + 100;
const handwrittenPortBase = portBase + 200;
const sampleCount = positiveInteger("BENCH_SAMPLES", 5);
const pingCount = positiveInteger("BENCH_PING_COUNT", 100_000);
const nonPipelinedPingCount = positiveInteger(
  "BENCH_NONPIPE_PING_COUNT",
  10_000,
);
const operationCount = positiveInteger("BENCH_OPERATION_COUNT", 10_000);
const concurrentClients = positiveInteger("BENCH_CONCURRENT_CLIENTS", 8);
const largeValueBytes = positiveInteger("BENCH_LARGE_VALUE", 12_000);
const randomSeed = 0x1234_5678;

function positiveInteger(name, fallback) {
  const value = Number.parseInt(process.env[name] ?? String(fallback), 10);
  if (!Number.isInteger(value) || value <= 0)
    throw new Error(`${name} must be a positive integer`);
  return value;
}

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

function buildSeconds(name) {
  const timing = readFileSync(`${directory}build/${name}.time`, "utf8");
  const match = /^real ([0-9.]+)$/m.exec(timing);
  if (match === null) throw new Error(`missing real time in ${name}.time`);
  return Number.parseFloat(match[1]);
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
  const pipelinedPing = await measurePipelinedPing(client);
  const nonPipelinedPing = await measureNonPipelinedPing(client);
  const randomSetGet = await measureRandomSetGet(client);
  const finalRssKiB = rssKiB(pid);
  return {
    pipelinedPingSeconds: pipelinedPing.seconds,
    pipelinedPingPerSecond: pipelinedPing.perSecond,
    nonPipelinedPingSeconds: nonPipelinedPing.seconds,
    nonPipelinedPingPerSecond: nonPipelinedPing.perSecond,
    randomSetSeconds: randomSetGet.setSeconds,
    randomSetPerSecond: randomSetGet.setPerSecond,
    randomGetSeconds: randomSetGet.getSeconds,
    randomGetPerSecond: randomSetGet.getPerSecond,
    initialRssKiB,
    finalRssKiB,
    rssGrowthKiB: finalRssKiB - initialRssKiB,
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

function summarize(samples) {
  const metricNames = [
    "startupMilliseconds",
    "pipelinedPingSeconds",
    "pipelinedPingPerSecond",
    "nonPipelinedPingSeconds",
    "nonPipelinedPingPerSecond",
    "randomSetSeconds",
    "randomSetPerSecond",
    "randomGetSeconds",
    "randomGetPerSecond",
    "initialRssKiB",
    "finalRssKiB",
    "rssGrowthKiB",
  ];
  return Object.fromEntries(
    metricNames.map((name) => {
      const values = samples.map((sample) => sample[name]);
      return [
        name,
        {
          min: Math.min(...values),
          median: median(values),
          max: Math.max(...values),
        },
      ];
    }),
  );
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
];

const results = [];
for (const implementation of allImplementations) {
  const samples = [];
  for (let sampleIndex = 0; sampleIndex < sampleCount; sampleIndex += 1) {
    const serverPort = implementation.basePort + sampleIndex;
    const childArguments = implementation.portArgument
      ? [...implementation.arguments, String(serverPort)]
      : implementation.arguments;
    const child = spawn(implementation.command, childArguments, {
      stdio: ["ignore", "pipe", "pipe"],
      env: {
        ...process.env,
        REDIS_NATIVE_PORT: String(serverPort),
        REDIS_PORT: String(serverPort),
      },
    });
    let stderr = "";
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
        samples.push({
          sample: sampleIndex + 1,
          port: serverPort,
          startupMilliseconds,
          ...(await benchmark(client, child.pid)),
        });
      } finally {
        client.close();
      }
    } finally {
      await stop(child);
    }
    if (child.exitCode && child.signalCode === null)
      throw new Error(`${implementation.name} failed: ${stderr}`);
  }
  results.push({
    name: implementation.name,
    artifactBytes: implementation.artifactBytes,
    samples,
    summary: summarize(samples),
  });
}

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
    pipelinedPingCount: pingCount,
    nonPipelinedPingCount,
    randomizedSetCount: operationCount,
    randomizedGetCount: operationCount,
    concurrentClients,
    largeValueBytes,
    randomSeed,
    portBases: {
      addon: addonPortBase,
      native: nativePortBase,
      handwritten: handwrittenPortBase,
    },
  },
  compilation: {
    addonSeconds: buildSeconds("build-addon"),
    nativeSeconds: buildSeconds("build-native"),
  },
  implementations: results,
};

writeFileSync(
  `${directory}results.json`,
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
