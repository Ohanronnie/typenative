#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
budget="$root/docs/performance-budgets.json"
protected="$root/benchmarks/json-parser/results.json"
protected_sha256=634c57f2f3b53be3bd51912b3321026a80f0099043b50f6dc0b53587d485634d

host="$(uname -s):$(uname -m)"
[ "$host" = "Darwin:arm64" ] || {
  printf '%s\n' "unsupported performance host: $host" >&2
  exit 2
}

llvm_config=${LLVM_CONFIG:-/opt/homebrew/opt/llvm/bin/llvm-config}
[ -x "$llvm_config" ] || {
  printf '%s\n' "LLVM 22.1.8 llvm-config not found: $llvm_config" >&2
  exit 2
}
[ "$($llvm_config --version)" = "22.1.8" ] || {
  printf '%s\n' "unsupported LLVM version: $($llvm_config --version)" >&2
  exit 2
}
command -v python3 >/dev/null 2>&1 || {
  printf '%s\n' 'python3 is required for machine-validated performance evidence' >&2
  exit 2
}
command -v node >/dev/null 2>&1 || {
  printf '%s\n' 'node is required for machine-validated performance evidence' >&2
  exit 2
}

target_dir=${CARGO_TARGET_DIR:-$root/target}
case "$target_dir" in
  /*) ;;
  *) target_dir="$root/$target_dir" ;;
esac
tn_bin=${TN_BIN:-"$target_dir/debug/tn"}
if [ ! -x "$tn_bin" ]; then
  tn_bin=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json, sys; print(json.load(sys.stdin)["target_directory"] + "/debug/tn")')
fi
[ -x "$tn_bin" ] || {
  printf '%s\n' "tn compiler not found: $tn_bin" >&2
  exit 2
}
guard="$root/scripts/tn-guarded.sh"
[ -x "$guard" ] || {
  printf '%s\n' "TypeNative compiler guard not found: $guard" >&2
  exit 2
}

current_sha256=$(shasum -a 256 "$protected" | awk '{print $1}')
[ "$current_sha256" = "$protected_sha256" ] || {
  printf '%s\n' "protected benchmark hash changed before performance run: $current_sha256" >&2
  exit 1
}

evidence=$(/usr/bin/mktemp -d /tmp/typenative-performance.XXXXXX)
json_report="$evidence/json.json"
redis_report="$evidence/redis.json"
http_report="$evidence/http.json"
allocation_binary="$evidence/allocation"
redis_allocation_binary="$evidence/redis-allocation"

run_logged() {
  label=$1
  shift
  if "$@" >"$evidence/$label.log" 2>&1; then
    return 0
  else
    status=$?
  fi
  printf '%s\n' "performance-$label=fail evidence=$evidence/$label.log" >&2
  sed -n '1,160p' "$evidence/$label.log" >&2
  exit "$status"
}

run_logged json env -u TYPENATIVE_RUNTIME_OBJECT \
  BENCH_ITERATIONS=1000 \
  BENCH_SAMPLES=9 \
  BENCH_WARMUPS=2 \
  BENCH_SHUFFLE_SEED=305419896 \
  BENCH_RESULTS="$json_report" \
  TN_BIN="$tn_bin" \
  "$root/benchmarks/json-parser/run.sh"

run_logged redis env -u TYPENATIVE_RUNTIME_OBJECT \
  BENCH_RESULTS="$redis_report" \
  BENCH_SAMPLES=9 \
  BENCH_WARMUPS=2 \
  BENCH_SHUFFLE_SEED=324508639 \
  BENCH_PING_COUNT=100000 \
  BENCH_NONPIPE_PING_COUNT=10000 \
  BENCH_OPERATION_COUNT=10000 \
  BENCH_CONCURRENT_CLIENTS=8 \
  BENCH_LARGE_VALUE=12000 \
  TN_BIN="$tn_bin" \
  "$root/benchmarks/redis-comparison/run.sh"

run_logged http env -u TYPENATIVE_RUNTIME_OBJECT \
  BENCH_FIXTURE_MIB=1 \
  BENCH_ITERATIONS=2 \
  BENCH_SAMPLES=5 \
  BENCH_RESULTS="$http_report" \
  TN_BIN="$tn_bin" \
  "$root/benchmarks/http-log-analyzer/run.sh"

run_logged allocation-build env -u TYPENATIVE_RUNTIME_OBJECT \
  TYPENATIVE_RUNTIME_ROOT="$root" \
  TYPENATIVE_TN_BIN="$tn_bin" \
  "$guard" "$tn_bin" build "$root/benchmarks/performance/allocation.tn" \
  --profile optimized --emit executable --out "$allocation_binary"

allocation_status=0
if "$allocation_binary" >"$evidence/allocation.log" 2>&1; then
  allocation_status=0
else
  allocation_status=$?
fi

run_logged redis-allocation-build env -u TYPENATIVE_RUNTIME_OBJECT \
  TYPENATIVE_RUNTIME_ROOT="$root" \
  TYPENATIVE_TN_BIN="$tn_bin" \
  "$guard" "$tn_bin" build "$root/validation/redis/allocation.tn" \
  --profile optimized --emit executable --out "$redis_allocation_binary"

redis_allocation_status=0
if "$redis_allocation_binary" >"$evidence/redis-allocation.log" 2>&1; then
  redis_allocation_status=0
else
  redis_allocation_status=$?
fi

python3 - "$budget" "$json_report" "$redis_report" "$http_report" "$allocation_status" "$redis_allocation_status" "$evidence/summary.json" <<'PY'
import json
import math
import statistics
import sys

budget_path, json_path, redis_path, http_path, allocation_text, redis_allocation_text, summary_path = sys.argv[1:]
with open(budget_path, encoding="utf-8") as stream:
    budget = json.load(stream)
with open(json_path, encoding="utf-8") as stream:
    json_report = json.load(stream)
with open(redis_path, encoding="utf-8") as stream:
    redis_report = json.load(stream)
with open(http_path, encoding="utf-8") as stream:
    http_report = json.load(stream)

def fail(message):
    raise SystemExit(f"performance verification failed: {message}")

def require(condition, message):
    if not condition:
        fail(message)

def median(values):
    ordered = sorted(values)
    middle = len(ordered) // 2
    return ordered[middle] if len(ordered) % 2 else (ordered[middle - 1] + ordered[middle]) / 2

def confidence(values):
    require(len(values) >= 5, "every measured metric needs at least five samples")
    mean = statistics.fmean(values)
    deviation = statistics.stdev(values) if len(values) > 1 else 0.0
    margin = 2.306 * deviation / math.sqrt(len(values))
    return {"count": len(values), "mean": mean, "median": median(values), "ci95": [mean - margin, mean + margin]}

def implementation_key(name):
    if name.startswith("Rust"):
        return "rust"
    if name.startswith("TypeNative") and ("executable" in name or "native" in name):
        return "native"
    if ".node" in name:
        return "addon"
    if "JSON.parse" in name:
        return "builtin"
    if "JavaScript" in name or "Node.js" in name:
        return "javascript"
    fail(f"unrecognized implementation name: {name}")

def implementation_map(items):
    output = {}
    for item in items:
        key = implementation_key(item["name"])
        require(key not in output, f"duplicate implementation {key}")
        output[key] = item
    return output

sampling = budget["sampling"]
json_workload = budget["workloads"]["json"]
json_env = json_report["environment"]
require(json_env["fixtureBytes"] == json_workload["fixtureBytes"], "JSON fixture size changed")
require(json_env["iterations"] == json_workload["iterations"], "JSON iteration workload changed")
require(json_env["samples"] == sampling["json"]["samples"], "JSON sample count changed")
require(json_env["warmups"] == sampling["json"]["warmups"], "JSON warmup count changed")
require(json_env["shuffleSeed"] == sampling["json"]["shuffleSeed"], "JSON shuffle seed changed")
require(len(json_report["methodology"]["warmupPlan"]) == 8, "JSON warmup plan is not two rounds")
require(len(json_report["methodology"]["measuredPlan"]) == 36, "JSON measured plan is not nine samples per product")
json_items = implementation_map(json_report["results"])
require(set(json_items) == {"native", "addon", "javascript", "builtin"}, "JSON products are incomplete")
json_evidence = {"samples": sampling["json"]["samples"], "warmups": sampling["json"]["warmups"], "products": {}}
for key, item in json_items.items():
    throughput = item["throughputMiBPerSecond"]
    require(throughput >= json_workload["minimumThroughputMiBPerSecond"][key], f"JSON {key} throughput budget exceeded")
    json_evidence["products"][key] = {
        "throughputMiBPerSecond": throughput,
        "timingMilliseconds": item["medianMilliseconds"],
    }
require(json_items["native"]["medianMilliseconds"] <= json_workload["maximumNativeWallMilliseconds"], "JSON native startup wall budget exceeded")

redis_workload = budget["workloads"]["redis"]
redis_env = redis_report["workload"]
for field in ("pipelinedPingCount", "nonPipelinedPingCount", "concurrentClients", "largeValueBytes", "internalTrials"):
    require(redis_env[field] == redis_workload[field], f"Redis {field} workload changed")
require(redis_env["randomizedSetCount"] == redis_workload["operationCount"], "Redis SET workload changed")
require(redis_env["randomizedGetCount"] == redis_workload["operationCount"], "Redis GET workload changed")
require(redis_env["samples"] == sampling["redis"]["samples"], "Redis sample count changed")
require(redis_env["warmups"] == sampling["redis"]["warmups"], "Redis warmup count changed")
require(redis_env["shuffleSeed"] == sampling["redis"]["shuffleSeed"], "Redis shuffle seed changed")
require(len(redis_report["methodology"]["warmupPlan"]) == 8, "Redis warmup plan is not two rounds")
require(len(redis_report["methodology"]["measuredPlan"]) == 36, "Redis measured plan is not nine samples per product")
require(redis_report["correctness"]["checkedBeforeTiming"], "Redis correctness checksum was not checked before timing")
require(len(redis_report["correctness"]["responseChecksumSha256"]) == 64, "Redis correctness checksum is malformed")
redis_items = implementation_map(redis_report["implementations"])
require(set(redis_items) == {"native", "addon", "rust", "javascript"}, "Redis products are incomplete")
redis_evidence = {"samples": sampling["redis"]["samples"], "warmups": sampling["redis"]["warmups"], "products": {}}
for key, item in redis_items.items():
    require(len(item["samples"]) == sampling["redis"]["samples"], f"Redis {key} samples are incomplete")
    summary = item["summary"]
    require(summary["pipelinedPingPerSecond"]["median"] >= redis_workload["minimumPipelinedPingPerSecond"][key], f"Redis {key} pipelined throughput budget exceeded")
    require(summary["randomSetPerSecond"]["median"] >= redis_workload["minimumRandomSetPerSecond"][key], f"Redis {key} SET throughput budget exceeded")
    require(summary["randomGetPerSecond"]["median"] >= redis_workload["minimumRandomGetPerSecond"][key], f"Redis {key} GET throughput budget exceeded")
    require(summary["startupMilliseconds"]["median"] <= redis_workload["maximumStartupMilliseconds"][key], f"Redis {key} startup budget exceeded")
    require(summary["nonPipelinedPingLatencyMicroseconds"]["median"] <= redis_workload["maximumNonPipelinedLatencyMicroseconds"][key], f"Redis {key} latency budget exceeded")
    for metric in ("cpuUserNanoseconds", "cpuSystemNanoseconds", "machSystemCalls", "unixSystemCalls", "contextSwitches"):
        require(summary[metric]["median"] >= 0, f"Redis {key} {metric} evidence is invalid")
        require(len(summary[metric]["confidenceInterval95"]) == 2, f"Redis {key} {metric} confidence interval is missing")
    if key in ("native", "addon"):
        require(summary["rssGrowthKiB"]["max"] <= redis_workload["maximumTypeNativeRssGrowthKiB"], f"Redis {key} RSS growth budget exceeded")
    redis_evidence["products"][key] = {
        "pipelinedPingPerSecond": confidence([sample["pipelinedPingPerSecond"] for sample in item["samples"]]),
        "randomSetPerSecond": confidence([sample["randomSetPerSecond"] for sample in item["samples"]]),
        "randomGetPerSecond": confidence([sample["randomGetPerSecond"] for sample in item["samples"]]),
        "startupMilliseconds": confidence([sample["startupMilliseconds"] for sample in item["samples"]]),
        "rssGrowthKiB": confidence([sample["rssGrowthKiB"] for sample in item["samples"]]),
        "artifactBytes": item["artifactBytes"],
    }

native_summary = redis_items["native"]["summary"]
addon_summary = redis_items["addon"]["summary"]
javascript_summary = redis_items["javascript"]["summary"]
rust_summary = redis_items["rust"]["summary"]
require(native_summary["pipelinedPingPerSecond"]["median"] >= rust_summary["pipelinedPingPerSecond"]["median"] * redis_workload["minimumNativeRustPipelinedRatio"], "Redis native pipelined throughput is below 95% of Rust")
require(addon_summary["pipelinedPingPerSecond"]["median"] >= rust_summary["pipelinedPingPerSecond"]["median"] * redis_workload["minimumAddonRustPipelinedRatio"], "Redis addon pipelined throughput is below 90% of Rust")
require(native_summary["randomSetPerSecond"]["median"] >= rust_summary["randomSetPerSecond"]["median"] * redis_workload["minimumNativeRustSetRatio"], "Redis native SET throughput is below 95% of Rust")
require(native_summary["randomGetPerSecond"]["median"] >= rust_summary["randomGetPerSecond"]["median"] * redis_workload["minimumNativeRustGetRatio"], "Redis native GET throughput is below 95% of Rust")
require(native_summary["nonPipelinedPingLatencyMicroseconds"]["median"] <= rust_summary["nonPipelinedPingLatencyMicroseconds"]["median"] * redis_workload["maximumNativeRustLatencyRatio"], "Redis native non-pipelined latency exceeds Rust by more than 5%")
native_rust_ratio = redis_report["comparisons"]["nativeVersusRustPipelinedPing"]["aggregateRatio"]
require(native_rust_ratio["confidenceInterval95"][0] >= redis_workload["minimumNativeRustPipelinedRatio"], "Redis native/Rust paired throughput ratio does not establish the 5% margin at 95% confidence")
require(redis_items["native"]["artifactBytes"] < redis_items["rust"]["artifactBytes"], "Redis native binary is not smaller than Rust")
require(native_summary["pipelinedPingPerSecond"]["median"] >= javascript_summary["pipelinedPingPerSecond"]["median"], "Redis native pipelined median is below handwritten Node")
native_difference = redis_report["comparisons"]["nativeVersusHandwrittenPipelinedPing"]["difference"]
require(native_difference["confidenceInterval95"][1] >= 0, "Redis native is significantly slower than handwritten Node at 95% confidence")
require(native_summary["nonPipelinedPingPerSecond"]["median"] >= javascript_summary["nonPipelinedPingPerSecond"]["median"], "Redis native non-pipelined PING is below handwritten Node")
require(native_summary["randomSetPerSecond"]["median"] > javascript_summary["randomSetPerSecond"]["median"], "Redis native SET is not faster than handwritten Node")
require(native_summary["randomGetPerSecond"]["median"] > javascript_summary["randomGetPerSecond"]["median"], "Redis native GET is not faster than handwritten Node")
require(native_summary["initialRssKiB"]["median"] <= redis_workload["maximumNativeInitialRssKiB"], "Redis native initial RSS exceeds 3 MiB")
require(native_summary["rssGrowthKiB"]["median"] <= redis_workload["maximumTypeNativeRssGrowthKiB"], "Redis native median RSS growth exceeds 1 MiB")
require(addon_summary["pipelinedPingPerSecond"]["median"] >= javascript_summary["pipelinedPingPerSecond"]["median"], "Redis addon pipelined median is below handwritten Node")
require(addon_summary["nonPipelinedPingPerSecond"]["median"] > javascript_summary["nonPipelinedPingPerSecond"]["median"], "Redis addon non-pipelined PING is not faster than handwritten Node")
require(addon_summary["randomSetPerSecond"]["median"] > javascript_summary["randomSetPerSecond"]["median"], "Redis addon SET is not faster than handwritten Node")
require(addon_summary["randomGetPerSecond"]["median"] > javascript_summary["randomGetPerSecond"]["median"], "Redis addon GET is not faster than handwritten Node")
addon_growth = max(0, addon_summary["rssGrowthKiB"]["median"])
javascript_growth = max(0, javascript_summary["rssGrowthKiB"]["median"])
require(addon_growth * redis_workload["minimumAddonRssGrowthAdvantage"] <= javascript_growth, "Redis addon RSS growth is not at least 20x below handwritten Node")

http_workload = budget["workloads"]["http"]
http_env = http_report["environment"]
require(http_env["fixtureBytes"] == http_workload["fixtureBytes"], "HTTP fixture size changed")
require(http_env["iterations"] == http_workload["iterations"], "HTTP iteration workload changed")
require(http_env["samples"] == sampling["http"]["samples"], "HTTP sample count changed")
http_items = implementation_map(http_report["results"])
require(set(http_items) == {"native", "addon", "javascript"}, "HTTP products are incomplete")
http_evidence = {"samples": sampling["http"]["samples"], "products": {}}
for key, item in http_items.items():
    require(item["throughputMiBPerSecond"]["median"] >= http_workload["minimumThroughputMiBPerSecond"][key], f"HTTP {key} throughput budget exceeded")
    require(item["peakRssBytes"] <= http_workload["maximumPeakRssBytes"][key], f"HTTP {key} memory budget exceeded")
    if key == "native":
        require(item["processWallMilliseconds"]["median"] <= http_workload["maximumProcessWallMilliseconds"], "HTTP native startup wall budget exceeded")
    http_evidence["products"][key] = {
        "throughputMiBPerSecond": item["throughputMiBPerSecond"]["median"],
        "peakRssBytes": item["peakRssBytes"],
        "artifactBytes": item.get("artifactBytes"),
    }

for timing_name, limit in budget["compiler"]["maximumSeconds"].items():
    actual = http_report["compilerTimings"][timing_name]["realSeconds"]
    require(actual <= limit, f"compiler {timing_name} time budget exceeded")

allocation_status = int(allocation_text)
require(allocation_status <= budget["allocation"]["maximumCount"], f"allocation count {allocation_status} exceeds budget")
redis_allocation_status = int(redis_allocation_text)
require(redis_allocation_status == 0, f"Redis PING allocation proof returned {redis_allocation_status}")

for key, limit in budget["artifacts"]["redis"]["maximumBytes"].items():
    require(redis_items[key]["artifactBytes"] <= limit, f"Redis {key} binary-size budget exceeded")
for key, limit in budget["artifacts"]["http"]["maximumBytes"].items():
    require(http_items[key]["artifactBytes"] <= limit, f"HTTP {key} binary-size budget exceeded")

summary = {
    "platform": budget["platform"],
    "json": json_evidence,
    "redis": redis_evidence,
    "http": http_evidence,
    "compilerTimings": http_report["compilerTimings"],
    "allocationCount": allocation_status,
    "redisPingAllocationCount": redis_allocation_status,
    "confidenceMethod": "deterministic percentile-bootstrap 95% intervals; paired comparisons use the ratio of summed fixed-work durations",
}
with open(summary_path, "w", encoding="utf-8") as stream:
    json.dump(summary, stream, indent=2)
    stream.write("\n")
print(json.dumps({"json": json_evidence, "redis": redis_evidence, "http": http_evidence, "allocationCount": allocation_status, "redisPingAllocationCount": redis_allocation_status}, separators=(",", ":")))
PY

current_sha256=$(shasum -a 256 "$protected" | awk '{print $1}')
[ "$current_sha256" = "$protected_sha256" ] || {
  printf '%s\n' "protected benchmark hash changed during performance run: $current_sha256" >&2
  exit 1
}

printf '%s\n' "performance-budgets=pass evidence=$evidence summary=$evidence/summary.json"
