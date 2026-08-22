import { createRequire } from "node:module";
import { resolve } from "node:path";

const require = createRequire(import.meta.url);
const addon = require(resolve(process.argv[2]));
const port = Number.parseInt(process.argv[3] ?? "6389", 10);
const exitCode = await addon.serve(port);
process.exitCode = exitCode;
