#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
require_closed=0
if [ "${1:-}" = "--require-closed" ]; then
  require_closed=1
  shift
fi
[ "$#" -eq 2 ] || {
  echo "usage: $0 [--require-closed] ACTIVE_LEDGER FROZEN_DEBT" >&2
  exit 2
}

python3 - "$root" "$1" "$2" "$require_closed" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
active_path = root / sys.argv[2]
frozen_path = root / sys.argv[3]
require_closed = sys.argv[4] == "1"
required = {
    "id", "scope", "severity", "root_cause", "affected_files",
    "red_regression", "verification_command", "evidence", "disposition",
}
closed = {"implemented", "proven-not-a-defect"}
allowed_scopes = {"active", "frozen-selfhost"}
errors = []

def load(path):
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception as exc:
        errors.append(f"{path}: cannot parse JSON: {exc}")
        return {}

def validate(path, expected_scope):
    document = load(path)
    if document.get("format") != 1:
        errors.append(f"{path}: format must be 1")
    rows = document.get("rows")
    if not isinstance(rows, list) or not rows:
        errors.append(f"{path}: rows must be a non-empty array")
        return []
    seen = set()
    for row in rows:
        if not isinstance(row, dict):
            errors.append(f"{path}: every row must be an object")
            continue
        missing = sorted(required - row.keys())
        if missing:
            errors.append(f"{path}: {row.get('id', '<missing id>')} missing {', '.join(missing)}")
        row_id = row.get("id")
        if not isinstance(row_id, str) or not row_id:
            errors.append(f"{path}: row id must be a non-empty string")
        elif row_id in seen:
            errors.append(f"{path}: duplicate row id {row_id}")
        else:
            seen.add(row_id)
        if row.get("scope") != expected_scope:
            errors.append(f"{path}: {row_id} scope must be {expected_scope}")
        if row.get("scope") not in allowed_scopes:
            errors.append(f"{path}: {row_id} has an invalid scope")
        if not isinstance(row.get("affected_files"), list) or not row.get("affected_files"):
            errors.append(f"{path}: {row_id} affected_files must be a non-empty array")
        regression = row.get("red_regression")
        if not isinstance(regression, dict) or not regression.get("command") or not regression.get("expected"):
            errors.append(f"{path}: {row_id} red_regression must include command and expected")
        evidence = row.get("evidence")
        if not isinstance(evidence, dict) or not {"source", "llvm", "behavioral"}.issubset(evidence):
            errors.append(f"{path}: {row_id} evidence must include source, llvm, and behavioral")
        disposition = row.get("disposition")
        if expected_scope == "active" and disposition not in ("open", *closed):
            errors.append(f"{path}: {row_id} has invalid active disposition {disposition!r}")
        if expected_scope == "frozen-selfhost" and disposition != "open":
            errors.append(f"{path}: {row_id} frozen disposition must remain open")
        if require_closed and expected_scope == "active":
            if disposition not in closed:
                errors.append(f"{path}: {row_id} is not closed")
            else:
                if not evidence.get("source") or not evidence.get("behavioral"):
                    errors.append(f"{path}: {row_id} closed disposition lacks source/behavioral evidence")
                llvm = evidence.get("llvm", "")
                if not llvm or (llvm.startswith("not-applicable:") is False and len(llvm) < 3):
                    errors.append(f"{path}: {row_id} closed disposition lacks LLVM evidence or checked not-applicable reason")
    return rows

active_rows = validate(active_path, "active")
frozen_rows = validate(frozen_path, "frozen-selfhost")
all_ids = [row.get("id") for row in active_rows + frozen_rows if isinstance(row, dict)]
if len(all_ids) != len(set(all_ids)):
    errors.append("ledger IDs must be unique across active and frozen inventories")

if errors:
    for error in errors:
        print(f"ledger-verification-error: {error}", file=sys.stderr)
    raise SystemExit(1)

print(f"ledger-verification=pass active={len(active_rows)} frozen={len(frozen_rows)} require_closed={require_closed}")
PY
