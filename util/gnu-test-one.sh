#!/usr/bin/env bash
# Runs individual GNU coreutils tests against Dogfish in seconds, without
# configuring or building GNU: for iterating on a fix. The full, authoritative
# run is util/gnu-tests.sh (CI); this one approximates its environment.
#
#   util/gnu-test-one.sh tests/cat/cat-E.sh tests/misc/paste.pl ...
#   util/gnu-test-one.sh -v tests/cat/cat-E.sh      # print each failing log
#
# How it works: the pinned GNU source tree (checksum-verified, never
# configured) provides the test scripts and their harness. A fake build
# directory provides src/: Dogfish for every program it implements, the
# system's GNU coreutils for the rest, a getlimits stand-in and a config.h
# declaring the features of CI's Ubuntu build. Each test then runs exactly as
# `make check` would run it, as an unprivileged user (as nobody when invoked
# as root). Linux on x86-64 only (getlimits' values).
#
# Checked against CI on all 417 selected tests: no test passes here that fails
# there or the reverse. Some skip here that CI runs (ACLs, setgid directories,
# two groups, process-group kill); CI stays the authority.
#
# Prints one line per test (PASS/FAIL/SKIP/ERROR) and exits 1 if any did not
# pass or skip. Logs: $WORK/logs/<test>.log.
# Environment: DOGFISH (binary; default: build target/release/dogfish), WORK
# (default target/gnu-one), TEST_TIMEOUT (seconds per test, default 120), JOBS
# (parallel tests, default: nproc).

set -euo pipefail

GNU_VERSION=9.4
GNU_SHA256=ea613a4cf44612326e917201bbbcdfbd301de21ffc3b59b6e5c07e040b275e52
GNU_URL="https://ftp.gnu.org/gnu/coreutils/coreutils-${GNU_VERSION}.tar.xz"

verbose=false
[ "${1:-}" = "-v" ] && { verbose=true; shift; }
[ $# -gt 0 ] || { sed -n '2,20s/^# \{0,1\}//p' "$0"; exit 2; }

die() { echo "gnu-test-one: $*" >&2; exit 2; }
repo=$(cd "$(dirname "$0")/.." && pwd)
[ "$(uname -s)" = Linux ] || die "Linux only"
[ -n "${DOGFISH:-}" ] || (cd "$repo" && cargo build --release --locked --quiet)
dogfish=$(realpath "${DOGFISH:-$repo/target/release/dogfish}")
work=$(mkdir -p "${WORK:-$repo/target/gnu-one}" && cd "${WORK:-$repo/target/gnu-one}" && pwd)
timeout_s=${TEST_TIMEOUT:-120}
sys_timeout=$(command -v timeout) || die "need timeout(1) from the system"

# The source tree: tests, harness, build-aux. Never configured or built.
src="$work/coreutils-$GNU_VERSION"
if [ ! -f "$src/tests/init.sh" ]; then
  tarball="$work/coreutils-$GNU_VERSION.tar.xz"
  [ -f "$tarball" ] || curl -fsSL --retry 3 -o "$tarball" "$GNU_URL"
  echo "$GNU_SHA256  $tarball" | sha256sum -c --quiet - || die "checksum mismatch for $tarball"
  tar -xJf "$tarball" -C "$work"
fi

# The fake build directory, rebuilt every run so it always points at this
# Dogfish binary.
build="$work/build"
rm -rf -- "$build"
mkdir -p "$build/src" "$work/logs"
# Every program GNU can build (EXTRA_PROGRAMS); configure on Linux builds all
# but the single-binary and libstdbuf pseudo-programs.
programs=$(awk '/^am__EXEEXT_[123] =/ {on = 1} on {print; if (!/\\$/) on = 0}' "$src/Makefile.in" \
  | grep -o 'src/[][a-z0-9_]*' | sed 's,src/,,' | grep -vx 'coreutils\|libstdbuf' | sort -u | tr '\n' ' ')
dogfish_tools=$("$dogfish" install-links "$build/src" >/dev/null && ls "$build/src")
for p in $programs; do
  [ -e "$build/src/$p" ] && continue
  # type -P: the program on PATH, never a shell builtin (test, [, echo, ...).
  sys=$(type -P "$p" 2>/dev/null) && ln -s "$sys" "$build/src/$p"
done
for t in $dogfish_tools; do
  case " $programs " in *" $t "*) ;; *) rm -f "$build/src/$t" ;; esac
done
ginstall=$(type -P install) && ln -sfn "$ginstall" "$build/src/ginstall"

cat >"$build/src/getlimits" <<'EOF'
#!/bin/sh
# Stand-in for GNU's src/getlimits on x86-64 Linux.
cat <<'L'
CHAR_MAX=127
CHAR_OFLOW=128
CHAR_MIN=-128
CHAR_UFLOW=-129
SCHAR_MAX=127
SCHAR_OFLOW=128
SCHAR_MIN=-128
SCHAR_UFLOW=-129
UCHAR_MAX=255
UCHAR_OFLOW=256
SHRT_MAX=32767
SHRT_OFLOW=32768
SHRT_MIN=-32768
SHRT_UFLOW=-32769
INT_MAX=2147483647
INT_OFLOW=2147483648
INT_MIN=-2147483648
INT_UFLOW=-2147483649
UINT_MAX=4294967295
UINT_OFLOW=4294967296
LONG_MAX=9223372036854775807
LONG_OFLOW=9223372036854775808
LONG_MIN=-9223372036854775808
LONG_UFLOW=-9223372036854775809
ULONG_MAX=18446744073709551615
ULONG_OFLOW=18446744073709551616
SIZE_MAX=18446744073709551615
SIZE_OFLOW=18446744073709551616
SSIZE_MAX=9223372036854775807
SSIZE_OFLOW=9223372036854775808
SSIZE_MIN=-9223372036854775808
SSIZE_UFLOW=-9223372036854775809
TIME_T_MAX=9223372036854775807
TIME_T_OFLOW=9223372036854775808
TIME_T_MIN=-9223372036854775808
TIME_T_UFLOW=-9223372036854775809
UID_T_MAX=4294967295
UID_T_OFLOW=4294967296
GID_T_MAX=4294967295
GID_T_OFLOW=4294967296
PID_T_MAX=2147483647
PID_T_OFLOW=2147483648
PID_T_MIN=-2147483648
PID_T_UFLOW=-2147483649
OFF_T_MAX=9223372036854775807
OFF_T_OFLOW=9223372036854775808
OFF_T_MIN=-9223372036854775808
OFF_T_UFLOW=-9223372036854775809
INTMAX_MAX=9223372036854775807
INTMAX_OFLOW=9223372036854775808
INTMAX_MIN=-9223372036854775808
INTMAX_UFLOW=-9223372036854775809
UINTMAX_MAX=18446744073709551615
UINTMAX_OFLOW=18446744073709551616
FLT_MIN=1.1754944e-38
FLT_MAX=3.4028235e+38
DBL_MIN=2.2250738585072014e-308
DBL_MAX=1.7976931348623157e+308
LDBL_MIN=3.3621031431120935063e-4932
LDBL_MAX=1.189731495357231765e+4932
L
EOF
chmod +x "$build/src/getlimits"
cat >"$build/config.h" <<'EOF'
/* The features GNU's tests look up, as configured on CI's Ubuntu runner. */
#define HAVE_SETGROUPS 1
#define HAVE_UTIMENSAT 1
#define HAVE_LUTIMES 1
#define HAVE_LINKAT 1
#undef LINKAT_SYMLINK_NOTSUP
#define USE_XATTR 1
#define USE_ACL 1
#define HAVE_INOTIFY 1
EOF

# Run as an unprivileged user: GNU's tests behave differently as root.
as_user=()
if [ "$(id -u)" -eq 0 ]; then
  as_user=(setpriv --reuid=65534 --regid=65534 --clear-groups)
  chmod -R a+rX "$src" "$build"
  chmod 1777 "$work/logs" "$build"
  for d in "$repo" "$repo/target" "$(dirname "$dogfish")"; do chmod a+x "$d"; done
fi
tmp=$(mktemp -d)
chmod 1777 "$tmp"
trap 'rm -rf -- "$tmp"' EXIT

run_test() {
  local t=$1 log rc r runner
  log="$work/logs/${t//\//-}.log"
  rm -f "$log"
  case $t in
    *.pl) runner=(perl -w -I"$src/tests" -MCuSkip -MCoreutils "-MCuTmpdir qw($t)") ;;
    *.xpl) runner=(perl -T -w -I"$src/tests" -MCuSkip -MCoreutils "-MCuTmpdir qw($t)") ;;
    *) runner=(/bin/sh) ;;
  esac
  (
    # Tests make their scratch directories in the build directory, as with
    # GNU's out-of-tree builds; the source tree stays read-only.
    cd "$build"
    export VERSION=$GNU_VERSION abs_top_builddir="$build" abs_top_srcdir="$src" \
      abs_srcdir="$src" built_programs="$programs" fail=0 host_os=linux-gnu \
      host_triplet=x86_64-pc-linux-gnu srcdir="$src" top_srcdir="$src" \
      CONFIG_HEADER="$build/config.h" CU_TEST_NAME="coreutils-$GNU_VERSION,${t//\//-}" \
      CC=cc AWK=awk EGREP="grep -E" EXEEXT= MAKE=make PACKAGE_VERSION=$GNU_VERSION \
      PERL=perl SHELL=/bin/sh LC_ALL=C TMPDIR="$tmp" PATH="$build/src:$PATH" HOME="$tmp"
    . "$src/tests/lang-default"
    . "$src/tests/envvar-check"
    "${as_user[@]}" env "$sys_timeout" -k 10 "$timeout_s" "${runner[@]}" "$src/$t"
  ) >"$log" 2>&1 9>&2 && rc=0 || rc=$?
  case $rc in
    0) r=PASS ;;
    77) r=SKIP ;;
    99) r=ERROR ;;
    *) r=FAIL ;;
  esac
  echo "$r $t" >"$tmp/result.${t//\//-}"
}

# Up to JOBS tests at a time; results print in the order given.
jobs=${JOBS:-$(nproc 2>/dev/null || echo 2)}
tests=()
for t in "$@"; do
  t=${t#./}
  [ -f "$src/$t" ] || { echo "ERROR $t (no such test)"; exit 2; }
  # CuTmpdir makes the Perl tests' scratch directory beside $t, relative to
  # the build directory.
  mkdir -p "$build/$(dirname "$t")"
  chmod 1777 "$build/$(dirname "$t")"
  tests+=("$t")
done
for t in "${tests[@]}"; do
  while [ "$(jobs -rp | wc -l)" -ge "$jobs" ]; do wait -n || true; done
  run_test "$t" &
done
wait

status=0
for t in "${tests[@]}"; do
  read -r r _ <"$tmp/result.${t//\//-}"
  echo "$r $t"
  case $r in PASS|SKIP) ;; *) status=1 ;; esac
  if $verbose && [ "$r" != PASS ] && [ "$r" != SKIP ]; then
    sed 's/^/    /' "$work/logs/${t//\//-}.log" | grep -v '^    + ' | tail -40
  fi
done
exit $status
