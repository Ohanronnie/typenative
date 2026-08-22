import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const root = path.resolve(import.meta.dirname, "../..");
const manifestPath = path.join(import.meta.dirname, "coverage.json");
const manifest = JSON.parse(fs.readFileSync(manifestPath, "utf8"));
const allowed = new Set(manifest.statuses);
const entries = [];

for (const name of fs.readdirSync(path.join(root, "std")).filter((value) => value.endsWith(".tn")).sort()) {
  const moduleName = `std/${name.slice(0, -3)}`;
  const rule = manifest.modules[moduleName];
  if (!rule) throw new Error(`missing coverage rule for ${moduleName}`);
  if (!allowed.has(rule.status)) throw new Error(`invalid coverage status for ${moduleName}`);
  const source = fs.readFileSync(path.join(root, "std", name), "utf8");
  const lines = source.split(/\r?\n/);
  for (const [index, line] of lines.entries()) {
    const match = line.match(/\bexport\s+(?:async\s+)?(?:function|struct|class|interface|enum|type|const|static)\s+([A-Za-z_][A-Za-z0-9_]*)/);
    if (!match) continue;
    entries.push({
      module: moduleName,
      export: match[1],
      sourceLine: index + 1,
      status: rule.status,
      evidence: rule.evidence,
    });
  }
}

if (entries.length === 0) throw new Error("coverage scanner found no public standard-library exports");
const report = {
  schema: manifest.schema,
  generatedFrom: "std/*.tn",
  exportCount: entries.length,
  entries,
};
const outputDirectory = path.join(import.meta.dirname, "build");
fs.mkdirSync(outputDirectory, { recursive: true });
fs.writeFileSync(path.join(outputDirectory, "coverage-expanded.json"), `${JSON.stringify(report, null, 2)}\n`);
if (process.argv.includes("--check")) {
  process.stdout.write(`forge-std-coverage=pass exports=${entries.length}\n`);
} else {
  process.stdout.write(`${JSON.stringify(report, null, 2)}\n`);
}
