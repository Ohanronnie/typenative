import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { Worker } from "node:worker_threads";

const require = createRequire(import.meta.url);

const [exportsPath, classesPath, fallibleClassesPath, asyncPath] =
  process.argv.slice(2);
const exportsApi = require(exportsPath);
assert.equal(exportsApi.optional(4), 4);
assert.equal(exportsApi.optional(undefined), undefined);
assert.equal(exportsApi.bytesLength(new Uint8Array([1, 2, 3, 4])), 4n);
assert.equal(exportsApi.echoString("a\0b"), "a\0b");
assert.equal(exportsApi.vectorLength([1, 2, 3]), 3n);
assert.equal(exportsApi.fixedMiddle([10, 42, 30]), 42);
assert.deepEqual(Array.from(exportsApi.makeVector()), [7, 8, 9]);
assert.equal(exportsApi.echoI8(-128), -128);
assert.equal(exportsApi.echoI8(127), 127);
assert.throws(() => exportsApi.echoI8(128));
assert.equal(exportsApi.echoI16(-32768), -32768);
assert.equal(exportsApi.echoI16(32767), 32767);
assert.throws(() => exportsApi.echoI16(32768));
assert.equal(exportsApi.echoU8(0), 0);
assert.equal(exportsApi.echoU8(255), 255);
assert.throws(() => exportsApi.echoU8(-1));
assert.throws(() => exportsApi.echoU8(256));
assert.equal(exportsApi.echoU16(0), 0);
assert.equal(exportsApi.echoU16(65535), 65535);
assert.throws(() => exportsApi.echoU16(-1));
assert.throws(() => exportsApi.echoU16(65536));
assert.equal(exportsApi.echoF32(Math.fround(1.25)), Math.fround(1.25));
assert.ok(Number.isNaN(exportsApi.echoF32(Number.NaN)));
assert.equal(
  exportsApi.echoF32(Number.POSITIVE_INFINITY),
  Number.POSITIVE_INFINITY,
);
assert.equal(exportsApi.echoChar(0x1f642), 0x1f642);
assert.throws(() => exportsApi.echoChar(0xd800));
assert.throws(() => exportsApi.echoChar(0x110000));
assert.equal(exportsApi.echoI128(-(1n << 127n)), -(1n << 127n));
assert.equal(exportsApi.echoI128((1n << 127n) - 1n), (1n << 127n) - 1n);
assert.equal(exportsApi.echoU128((1n << 128n) - 1n), (1n << 128n) - 1n);
assert.throws(() => exportsApi.echoU128(-1n));

const classesApi = require(classesPath);
const counter = new classesApi.Counter(5);
assert.equal(counter.increment(7), 12);
if (typeof global.gc === "function") {
  const baseline = classesApi.freeCount();
  let values = [];
  for (let index = 0; index < 10000; index += 1) {
    values.push(new classesApi.Counter(index));
  }
  values = [];
  await new Promise((resolve) => {
    let rounds = 0;
    const collect = () => {
      for (let index = 0; index < 10; index += 1) global.gc();
      rounds += 1;
      if (rounds === 100) resolve();
      else setImmediate(collect);
    };
    collect();
  });
  assert.ok(classesApi.freeCount() >= baseline + 10000n);
}

const fallibleClassesApi = require(fallibleClassesPath);
const holder = new fallibleClassesApi.Holder();
assert.deepEqual(Array.from(holder.echo(new Uint8Array([7, 8, 9]))), [7, 8, 9]);
let syncError;
try {
  fallibleClassesApi.syncFail();
} catch (error) {
  syncError = error;
}
assert.ok(syncError);
assert.equal(syncError.name, "Failure");
assert.equal(syncError.typeNative, "Failure");
assert.equal(syncError.code, 17);

const asyncApi = require(asyncPath);
assert.equal(await asyncApi.immediate(41), 42);
assert.deepEqual(
  Array.from(await asyncApi.asyncEcho(new Uint8Array([3, 4]))),
  [3, 4],
);
await assert.rejects(asyncApi.asyncFail(41), (error) => {
  assert.equal(error.name, "Failure");
  assert.equal(error.typeNative, "Failure");
  assert.equal(error.code, 23);
  return true;
});

const worker = new Worker(
  `const { parentPort, workerData } = require("node:worker_threads");
   const { createRequire } = require("node:module");
   const requireFromFixture = createRequire(workerData.base);
   const api = requireFromFixture(workerData.addon);
   parentPort.postMessage(api.optional(9));`,
  { eval: true, workerData: { addon: exportsPath, base: import.meta.url } },
);
const workerResult = await new Promise((resolve, reject) => {
  worker.once("message", resolve);
  worker.once("error", reject);
});
assert.equal(workerResult, 9);
assert.equal(await new Promise((resolve) => worker.once("exit", resolve)), 0);

console.log("node-api-validation=pass");
