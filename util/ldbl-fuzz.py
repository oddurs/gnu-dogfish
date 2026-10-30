#!/usr/bin/env python3
"""Differential fuzzer for `long double`: Dogfish `printf` and `seq` vs GNU.

  ldbl-fuzz.py [--rounds N] [--seed S] [--dogfish PATH] [--gnu DIR]

Each round picks a random floating-point directive (`%a %A %e %E %f %F %g %G`
with random flags, width, precision and an optional `L`) and runs a batch of
random numbers through `printf 'DIRECTIVE|\\n' NUMBERS...` on both sides (one
process each), then a few `seq -f DIRECTIVE FIRST STEP LAST` (or `seq -w`)
runs. The numbers cover the whole range of the platform's `long double`:
decimal ones with up to 25 digits and any exponent (subnormals, overflow and
underflow included, which GNU reports as range errors), hexadecimal ones,
integers around powers of two, decimal halfway cases and specials.

Stdout, stderr and the exit status are compared byte for byte; a diverging
printf batch is narrowed to the first number that differs alone. Divergences
are printed with a command that reproduces them; the exit status is 1 if any
case diverged. Defaults: 300 rounds of 200 numbers, seed from the clock,
DOGFISH=target/release/dogfish (built if missing), GNU programs from /usr/bin.
"""

import argparse
import os
import random
import shlex
import subprocess
import sys
import time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
ENV = dict(os.environ, LC_ALL="C", LANG="C")
ENV.pop("POSIXLY_CORRECT", None)


def number(rng, max_exp):
    sign = rng.choice(["", "", "-", "+"])
    kind = rng.randrange(10)
    digits = lambda n: "".join(rng.choice("0123456789") for _ in range(n))
    if kind <= 3:
        n = rng.randint(1, 25)
        d = digits(n)
        p = rng.randint(0, n)
        body = "%s.%se%d" % (d[:p], d[p:], rng.randint(-max_exp, max_exp))
    elif kind == 4:
        body = "%s.%s" % (digits(rng.randint(1, 8)), digits(rng.randint(0, 8)))
    elif kind == 5:
        hexd = "".join(rng.choice("0123456789abcdef") for _ in range(rng.randint(1, 18)))
        body = "0x%sp%d" % (hexd, rng.randint(-3 * max_exp, 3 * max_exp))
    elif kind == 6:
        body = str((1 << rng.randrange(70)) + rng.randint(-2, 2))
    elif kind == 7:
        body = "%s.%s5" % (digits(rng.randint(1, 3)), digits(rng.randint(0, 6)))
    elif kind == 8:
        body = rng.choice(["inf", "nan", "0", "1", "0.5", "1e4932", "0x1p-16445",
                           "1e-4950", "1.18973149535723176502e+4932"])
    else:
        body = "0x%x.%xp%d" % (rng.randrange(16), rng.getrandbits(40), rng.randint(-100, 100))
    return sign + body


def directive(rng):
    conv = rng.choice("aAeEfFgG")
    d = "%" + "".join(f for f in "-+ #0" if rng.randrange(5) == 0)
    if rng.randrange(2):
        d += str(rng.randrange(30))
    if rng.randrange(3):
        d += "." + str(rng.randrange(20 if conv in "aA" else 40))
    if rng.randrange(2):
        d += "L"
    return d + conv, conv


def run(argv):
    p = subprocess.run(argv, capture_output=True, env=ENV, timeout=60)
    return p.stdout, p.stderr, p.returncode


def compare(dogfish, gnu, tool, args):
    ours = run([dogfish, tool] + args)
    theirs = run([os.path.join(gnu, tool)] + args)
    # GNU names itself by its path in diagnostics.
    theirs = (theirs[0], theirs[1].replace(os.path.join(gnu, tool).encode() + b":",
                                           tool.encode() + b":"), theirs[2])
    return ours == theirs, ours, theirs


def report(tool, args, ours, theirs):
    print("DIVERGES: %s %s" % (tool, " ".join(shlex.quote(a) for a in args)))
    for name, side in (("dogfish", ours), ("gnu", theirs)):
        print("  %s: status %d stdout %r stderr %r" % (name, side[2], side[0][:300], side[1][:300]))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--rounds", type=int, default=300)
    ap.add_argument("--numbers", type=int, default=200)
    ap.add_argument("--seed", type=int, default=None)
    ap.add_argument("--dogfish", default=os.environ.get("DOGFISH"))
    ap.add_argument("--gnu", default=os.environ.get("GNU", "/usr/bin"))
    opts = ap.parse_args()
    seed = opts.seed if opts.seed is not None else int(time.time())
    rng = random.Random(seed)
    dogfish = opts.dogfish
    if not dogfish:
        dogfish = os.path.join(ROOT, "target", "release", "dogfish")
        if not os.path.exists(dogfish):
            subprocess.run(["cargo", "build", "--release", "-q"], cwd=ROOT, check=True)
    print("seed %d" % seed)
    failures = 0
    cases = 0
    for _ in range(opts.rounds):
        d, conv = directive(rng)
        max_exp = 40 if conv in "fF" else 4960
        nums = [number(rng, max_exp) for _ in range(opts.numbers)]
        fmt = d + "|\\n"
        cases += len(nums)
        same, ours, theirs = compare(dogfish, opts.gnu, "printf", [fmt] + nums)
        if not same:
            failures += 1
            culprit = next((n for n in nums if not compare(dogfish, opts.gnu, "printf", [fmt, n])[0]), None)
            args = [fmt, culprit] if culprit is not None else [fmt] + nums
            _, ours, theirs = compare(dogfish, opts.gnu, "printf", args)
            report("printf", args, ours, theirs)
        for _ in range(4):
            first, step = number(rng, 6), number(rng, 6)
            try:
                f, s = float(first), float(step)
            except ValueError:
                continue
            # Steps of a similar size keep FIRST + i STEP moving.
            if f != f or s != s or abs(f) == float("inf") or abs(s) == float("inf") \
                    or f == 0 or abs(s) < abs(f) * 1e-9 or abs(s) < 1e-30:
                continue
            last = "%e" % (f + s * rng.randrange(20))
            args = ["-w" if rng.randrange(3) == 0 else "-f" + d, "--", first, step, last]
            cases += 1
            same, ours, theirs = compare(dogfish, opts.gnu, "seq", args)
            if not same:
                failures += 1
                report("seq", args, ours, theirs)
    print("%d cases, %d divergences (seed %d)" % (cases, failures, seed))
    sys.exit(1 if failures else 0)


if __name__ == "__main__":
    main()
