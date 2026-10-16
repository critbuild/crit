#!/bin/sh
# Profile-guided release build of crit. An instrumented build first runs
# scripts/pgo-workload.py (recorded agent output, repaints and a split, in a throwaway home),
# then the release build is laid out for the branches and calls that run measured. On real OMP
# output it never trained on, this cut crit's CPU per byte by about a quarter. The result is
# target/pgo/release/crit; plain `cargo build --release` is unaffected.
#
# Needs python3, and llvm-profdata from the same LLVM major version as rustc (rustup's
# llvm-tools component, or the system llvm package when rustc is the system one).
set -eu
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

llvm=$(rustc -vV | sed -n 's/^LLVM version: \([0-9]*\).*/\1/p')
host=$(rustc -vV | sed -n 's/^host: //p')
profdata=
for candidate in "$(rustc --print sysroot)/lib/rustlib/$host/bin/llvm-profdata" \
    "$(command -v "llvm-profdata-$llvm" || true)" "$(command -v llvm-profdata || true)"; do
    [ -x "$candidate" ] || continue
    if [ "$("$candidate" --version | sed -n 's/.*LLVM version \([0-9]*\).*/\1/p' | head -n 1)" = "$llvm" ]; then
        profdata=$candidate
        break
    fi
done
if [ -z "$profdata" ]; then
    echo "pgo-build: needs llvm-profdata from LLVM $llvm, the version rustc uses" >&2
    exit 1
fi

# The instrumented build keeps a stable RUSTFLAGS, so its dependencies are built once. The
# profile's path changes every run, so the optimized build, dependencies included (the vt100
# parser is one), is always rebuilt against the profile just measured.
generate=$root/target/pgo-generate
work=$(mktemp -d "${TMPDIR:-/var/tmp}/crit-pgo.XXXXXX")
trap 'rm -rf "$work"' EXIT INT TERM
rm -rf "$generate/profiles"

RUSTFLAGS="-Cprofile-generate=$generate/profiles" \
    cargo build --release --target-dir "$generate"
python3 scripts/pgo-workload.py "$generate/release/crit" "$work"
"$profdata" merge -o "$work/crit.profdata" "$generate/profiles"
RUSTFLAGS="-Cprofile-use=$work/crit.profdata -Cllvm-args=-pgo-warn-missing-function=false" \
    cargo build --release --target-dir "$root/target/pgo"
echo "$root/target/pgo/release/crit"
