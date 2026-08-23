#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

fail_source_scan() {
  pattern=$1
  description=$2
  matches=$(rg -n --hidden \
    --glob '!scripts/bootstrap-self-host.sh' \
    --glob '!scripts/check-direct-llvm-backend.sh' \
    --glob '!compiler-tn/**' \
    --glob '!target/**' \
    --glob '!build/**' \
    --glob '!.git/**' \
    "$pattern" \
    "$root/crates" "$root/runtime" "$root/std" "$root/validation" "$root/tests" "$root/benchmarks" "$root/scripts" \
    2>/dev/null || true)
  if [ -n "$matches" ]; then
    printf '%s\n' "$matches" >&2
    echo "direct-llvm regression scan matched $description" >&2
    exit 1
  fi
}

fail_source_scan 'node_wrapper_source|node_include_directory|write_node_(wrapper|class|async|argument|result)|node_completion_name|node_abi_payload_expression|class_drop_symbol' 'the removed generated Node C renderer'
fail_source_scan 'TYPENATIVE_C_SOURCE_DUMP|generic_codegen|materializeSource|normalizeCompoundAssignments|\.tn\.c' 'a generated-C compiler path'
fail_source_scan 'clang[^[:cntrl:]]*(\.c|[[:space:]]-c[[:space:]]|--language=c|-std=gnu11)' 'a generated-C Clang subprocess'
fail_source_scan '(^|[^[:alnum:]_])system[[:space:]]*\(' 'system()'
fail_source_scan '(^|[^[:alnum:]_])popen[[:space:]]*\(' 'popen()'
fail_source_scan 'Command::new\("(sh|bash|zsh|/bin/sh|/bin/bash)"\)|\.arg\("-c"\)' 'shell command construction'
fail_source_scan 'node_api\.h|NAPI_MODULE_INIT\(' 'generated Node C source'

if [ "$#" -gt 0 ]; then
  artifact_root=$1
  [ -d "$artifact_root" ] || {
    echo "artifact root does not exist: $artifact_root" >&2
    exit 2
  }
  generated=$(find "$artifact_root" -type f \( \
    -name '*.c' -o -name '*.cc' -o -name '*.cpp' -o -name '*.cxx' -o \
    -name '*.tn.c' -o -name '*.tn.debug.c' \
  \) -print -quit)
  if [ -n "$generated" ]; then
    echo "direct LLVM regression scan found generated C source: $generated" >&2
    exit 1
  fi
fi

printf '%s\n' 'direct-llvm-regression-scan=pass'
