import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { Worker } from "node:worker_threads";

const require = createRequire(import.meta.url);

const [exportsPath, classesPath, fallibleClassesPath, asyncPath] = process.argv.slice(2);
const exportsApi = require(exportsPath);
assert.equal(exportsApi.optional(4), 4);
assert.equal(exportsApi.optional(undefined), undefined);
assert.equal(exportsApi.bytesLength(new Uint8Array([1, 2, 3, 4])), 4n);
assert.equal(exportsApi.vectorLength([1, 2, 3]), 3n);
assert.equal(exportsApi.fixedMiddle([10, 42, 30]), 42);

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

const asyncApi = require(asyncPath);
assert.equal(await asyncApi.immediate(41), 42);
assert.deepEqual(Array.from(await asyncApi.asyncEcho(new Uint8Array([3, 4]))), [3, 4]);
await assert.rejects(asyncApi.asyncFail(41));

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
