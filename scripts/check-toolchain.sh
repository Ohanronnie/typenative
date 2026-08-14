#!/bin/sh
set -eu

required_version=22.1.8

if [ -n "${LLVM_SYS_221_PREFIX:-}" ] && [ -x "${LLVM_SYS_221_PREFIX}/bin/llvm-config" ]; then
  llvm_config="${LLVM_SYS_221_PREFIX}/bin/llvm-config"
elif [ -x /opt/homebrew/opt/llvm/bin/llvm-config ]; then
  llvm_config=/opt/homebrew/opt/llvm/bin/llvm-config
elif [ -x /usr/local/opt/llvm/bin/llvm-config ]; then
  llvm_config=/usr/local/opt/llvm/bin/llvm-config
elif command -v llvm-config-22 >/dev/null 2>&1; then
  llvm_config=$(command -v llvm-config-22)
elif command -v llvm-config >/dev/null 2>&1; then
  llvm_config=$(command -v llvm-config)
else
  echo "LLVM_TOOLCHAIN_NOT_FOUND: LLVM ${required_version} llvm-config was not found" >&2
  exit 1
fi

actual_version=$($llvm_config --version)
if [ "$actual_version" != "$required_version" ]; then
  echo "LLVM_TOOLCHAIN_VERSION_MISMATCH: expected ${required_version}, found ${actual_version}" >&2
  exit 1
fi

prefix=$($llvm_config --prefix)
case $(uname -s):$(uname -m) in
  Darwin:arm64) target=aarch64-apple-darwin ;;
  Linux:x86_64) target=x86_64-unknown-linux-gnu ;;
  *)
    echo "LLVM_TOOLCHAIN_UNSUPPORTED_HOST: $(uname -s) $(uname -m)" >&2
    exit 1
    ;;
esac

printf 'llvm_version=%s\nllvm_prefix=%s\nhost_target=%s\n' "$actual_version" "$prefix" "$target"
