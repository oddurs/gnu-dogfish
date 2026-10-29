#!/usr/bin/env bash
# Runs GNU coreutils' own test suite against Dogfish and reports a pass rate.
#
#   util/gnu-tests.sh                    build GNU, swap in Dogfish, run, report
#   util/gnu-tests.sh --update-baseline  also rewrite util/gnu-baseline.json
#
# How it works: build the pinned GNU release once (its tests need the tree and
# helper programs), replace every program in src/ that Dogfish implements with
# a symlink to the Dogfish multicall binary, then run only the tests whose
# programs under test (their `print_ver_` line) are all Dogfish's. Programs
# Dogfish does not implement stay GNU, and their tests are not counted.
#
# Must not run as root: many GNU tests skip or behave differently as root.
# Environment: DOGFISH (binary; default: build target/release/dogfish), WORK
# (scratch dir, default target/gnu), JOBS (default: nproc), TEST_TIMEOUT
# (seconds per test, default 600), TESTS (override the selected test list).

set -euo pipefail

GNU_VERSION=9.4
# Verified against the release signature (Pádraig Brady, GNU keyring).
GNU_SHA256=ea613a4cf44612326e917201bbbcdfbd301de21ffc3b59b6e5c07e040b275e52
GNU_URL="https://ftp.gnu.org/gnu/coreutils/coreutils-${GNU_VERSION}.tar.xz"

repo=$(cd "$(dirname "$0")/.." && pwd)
# Without DOGFISH, test a fresh release build of this checkout.
[ -n "${DOGFISH:-}" ] || (cd "$repo" && cargo build --release --locked --quiet)
dogfish=$(realpath "${DOGFISH:-$repo/target/release/dogfish}")
work=$(mkdir -p "${WORK:-$repo/target/gnu}" && cd "${WORK:-$repo/target/gnu}" && pwd)
jobs=${JOBS:-$(nproc 2>/dev/null || echo 2)}
per_test_timeout=${TEST_TIMEOUT:-600}
update_baseline=false
[ "${1:-}" = "--update-baseline" ] && update_baseline=true

die() { echo "gnu-tests: $*" >&2; exit 1; }
[ "$(id -u)" -ne 0 ] || die "run as an unprivileged user; GNU's tests behave differently as root"
[ -x "$dogfish" ] || die "no Dogfish binary at $dogfish (cargo build --release)"
# The harness puts src/ first on PATH, so the per-test timeout must be the
# system's by absolute path, never the Dogfish one under test.
sys_timeout=$(command -v timeout) || die "need timeout(1) from the system"

src="$work/coreutils-$GNU_VERSION"
if [ ! -x "$src/src/ls.gnu" ] && [ ! -x "$src/src/ls" ]; then
  tarball="$work/coreutils-$GNU_VERSION.tar.xz"
  [ -f "$tarball" ] || curl -fsSL --retry 3 -o "$tarball" "$GNU_URL"
  echo "$GNU_SHA256  $tarball" | sha256sum -c --quiet - || die "checksum mismatch for $tarball"
  rm -rf "$src"
  tar -xJf "$tarball" -C "$work"
  echo "gnu-tests: building GNU coreutils $GNU_VERSION (once)"
  (cd "$src" && ./configure --quiet --disable-nls >"$work/configure.log" 2>&1 \
     && make -j"$jobs" >"$work/build.log" 2>&1) \
    || die "GNU build failed; see $work/configure.log and $work/build.log"
fi

# Swap Dogfish in for every program it implements (keeping GNU's as *.gnu).
links=$(mktemp -d)
trap 'rm -rf "$links"' EXIT
"$dogfish" install-links "$links" >/dev/null
tools_file="$work/dogfish-tools.txt"
: >"$tools_file"
for link in "$links"/*; do
  tool=$(basename "$link")
  [ -e "$src/src/$tool" ] || [ -e "$src/src/$tool.gnu" ] || continue
  [ -e "$src/src/$tool.gnu" ] || mv "$src/src/$tool" "$src/src/$tool.gnu"
  ln -sfn "$dogfish" "$src/src/$tool"
  echo "$tool" >>"$tools_file"
done
echo "gnu-tests: Dogfish provides $(wc -l <"$tools_file") GNU programs"

# Every swapped program must really be Dogfish; otherwise the run would quietly
# test GNU against itself and report a perfect score.
assert_swapped() {
  local tool bad=""
  while read -r tool; do
    # Capture first: `false --version` prints its version and still exits 1.
    local banner
    banner=$("$src/src/$tool" --version 2>/dev/null || true)
    [ -L "$src/src/$tool" ] && [[ "$banner" == *GNU/Dogfish* ]] || bad="$bad $tool"
  done <"$tools_file"
  [ -z "$bad" ] || die "$1: not Dogfish:$bad"
}
assert_swapped "before the run"

helper="$repo/util/gnu_tests.py"
mapping="$work/selection.json"
python3 -I "$helper" select "$src" "$tools_file" "$mapping"
selected=${TESTS:-$(python3 -I "$helper" names "$mapping")}
echo "gnu-tests: running $(wc -w <<<"$selected") tests with $jobs jobs"

# check-TESTS runs the harness without `all`, so make never relinks the
# programs we replaced. Old logs are removed so results are from this run.
(cd "$src" && find tests -name '*.log' -o -name '*.trs' | xargs rm -f)
# The swapped programs look out of date to make (GNU's objects are newer than
# the Dogfish binary), so mark each one "old" or make relinks GNU's over them.
keep_old=()
while read -r tool; do keep_old+=(-o "src/$tool"); done <"$tools_file"
(cd "$src" && make -k -j"$jobs" "${keep_old[@]}" check-TESTS \
    TESTS="$selected" VERBOSE=no \
    SH_LOG_COMPILER="$sys_timeout -k 10 $per_test_timeout \$(SHELL)" \
    PL_LOG_COMPILER="$sys_timeout -k 10 $per_test_timeout \$(TESTSUITE_PERL) \$(TESTSUITE_PERL_OPTIONS)" \
    >"$work/check.log" 2>&1) || true
grep -q 'CCLD' "$work/check.log" && echo "gnu-tests: warning: make relinked something; see check.log" >&2
assert_swapped "after the run"

report_args=("$src" "$mapping" "$work/gnu-results.json" --summary "$work/gnu-summary.md"
             --baseline "$repo/util/gnu-baseline.json")
$update_baseline && report_args+=(--update-baseline)
python3 -I "$helper" report "${report_args[@]}"
