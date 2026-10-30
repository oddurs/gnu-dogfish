#!/usr/bin/env python3
"""Head-to-head benchmarks: Dogfish against GNU coreutils on the same inputs.

  util/bench.py                      build Dogfish, run every benchmark, print a table
  util/bench.py -k sort -k wc        only benchmarks whose name contains a pattern
  util/bench.py --json out.json      also write the results
  util/bench.py --baseline b.json    fail if a ratio is more than 15% worse than in b.json
  util/bench.py --history h.ndjson   compare against earlier runs on the same CPU, then
                                     append this run (what CI does on every merge to main)
  util/bench.py --self-test          check that the regression test catches a 20% slowdown

Each benchmark runs the same command line with the GNU program (/usr/bin, or
GNU= directory) and with Dogfish (through a symlink named like the tool, as
installed), alternating, REPEAT times (default 7). Reported per side: median
wall time and median CPU time (user + system, from wait4), and the ratio
GNU/Dogfish: above 1.0 Dogfish is faster. Both run with LC_ALL=C (Dogfish
collates bytes as GNU does in the C locale), stdout to a regular file
(truncated each run, as `> file` would; /dev/null would let a tool skip its
writes), stdin from the named input.

Inputs are generated deterministically under WORK (default target/bench) once.
Environment: DOGFISH (binary; default: build target/release/dogfish), GNU
(directory with GNU's programs, default /usr/bin), REPEAT, WORK.
"""

import argparse
import datetime
import json
import os
import platform
import random
import shutil
import statistics
import subprocess
import sys
import time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

WORDS = ("alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu nu xi "
         "omicron pi rho sigma tau upsilon phi chi psi omega error warn info debug "
         "GET POST /api/v1/users /static/app.js 200 404 500 user=42 latency_ms").split()


def gen_inputs(work):
    """Writes the benchmark inputs once; returns their paths."""
    os.makedirs(work, exist_ok=True)
    rng = random.Random(1234)
    paths = {}

    def write(name, size, line):
        p = os.path.join(work, name)
        paths[name] = p
        if os.path.exists(p) and os.path.getsize(p) >= size:
            return
        with open(p + ".tmp", "w") as f:
            n = 0
            while n < size:
                s = line()
                f.write(s)
                n += len(s)
        os.replace(p + ".tmp", p)

    # ~100 MiB of log-like text, ~60 bytes a line.
    write("log.txt", 100 << 20,
          lambda: "%d %s %s %s %d\n" % (rng.randrange(1_000_000_000), rng.choice(WORDS),
                                        rng.choice(WORDS), rng.choice(WORDS), rng.randrange(10000)))
    # ~20 MiB of numbers, one per line (sort -n/-g, uniq after sort).
    write("nums.txt", 20 << 20, lambda: "%d\n" % rng.randrange(-10**9, 10**9))
    # ~20 MiB of CSV for cut.
    write("data.csv", 20 << 20,
          lambda: ",".join(str(rng.randrange(10**6)) for _ in range(8)) + "\n")
    # Sorted lines with many duplicates for uniq.
    p = os.path.join(work, "dups.txt")
    paths["dups.txt"] = p
    if not os.path.exists(p):
        lines = ["%s %d\n" % (rng.choice(WORDS), rng.randrange(2000)) for _ in range(1_000_000)]
        lines.sort()
        with open(p, "w") as f:
            f.writelines(lines)
    # 256 MiB of random bytes for checksums, base64, cp.
    p = os.path.join(work, "blob.bin")
    paths["blob.bin"] = p
    if not os.path.exists(p) or os.path.getsize(p) < 256 << 20:
        with open(p + ".tmp", "wb") as f:
            for _ in range(256):
                f.write(rng.randbytes(1 << 20))
        os.replace(p + ".tmp", p)
    p = os.path.join(work, "blob.b64")
    paths["blob.b64"] = p
    if not os.path.exists(p):
        subprocess.run(["/usr/bin/base64", paths["blob.bin"]], stdout=open(p, "wb"), check=True)
    # A tree of 30,000 files in 300 directories.
    t = os.path.join(work, "tree")
    paths["tree"] = t
    if not os.path.exists(os.path.join(t, ".done")):
        shutil.rmtree(t, ignore_errors=True)
        for d in range(300):
            dd = os.path.join(t, "d%03d" % (d // 30), "s%03d" % d)
            os.makedirs(dd)
            for i in range(100):
                with open(os.path.join(dd, "f%03d.txt" % i), "w") as f:
                    f.write("x" * rng.randrange(1, 4096))
        open(os.path.join(t, ".done"), "w").close()
    small = os.path.join(work, "small.txt")
    paths["small.txt"] = small
    if not os.path.exists(small):
        with open(small, "w") as f:
            f.write("hello world\n" * 10)
    return paths


def benchmarks(p, scratch):
    """(name, tool, argv-after-tool, stdin path or None, setup per run or None, invocations).

    argv may be {"gnu": [...], "dogfish": [...]} when the two sides differ."""
    L, N, C, D, B = p["log.txt"], p["nums.txt"], p["data.csv"], p["dups.txt"], p["blob.bin"]
    T = p["tree"]
    out = os.path.join(scratch, "out")

    def fresh(path):
        return lambda: shutil.rmtree(path, ignore_errors=True) if os.path.isdir(path) else (
            os.path.exists(path) and os.remove(path))

    def tree_copy():
        fresh(out)()  # a file left by "cp 256M" when -k skips "cp -r"
        subprocess.run(["/usr/bin/cp", "-r", T, out], check=True)

    return [
        # Startup: what a shell script pays per call.
        ("startup: true x200", "true", [], None, None, 200),
        ("startup: basename x200", "basename", ["/usr/lib/x.so", ".so"], None, None, 200),
        ("startup: cat small x200", "cat", [p["small.txt"]], None, None, 200),
        # Streaming text.
        ("cat 100M", "cat", [L], None, None, 1),
        ("cat -n 100M", "cat", ["-n", L], None, None, 1),
        ("wc 100M", "wc", [L], None, None, 1),
        ("wc -l 100M", "wc", ["-l", L], None, None, 1),
        ("wc -c 100M", "wc", ["-c", L], None, None, 1),
        ("wc -w 100M", "wc", ["-w", L], None, None, 1),
        ("wc -m 100M", "wc", ["-m", L], None, None, 1),
        ("head -n 1M", "head", ["-n", "1000000", L], None, None, 1),
        ("tail -n 1M", "tail", ["-n", "1000000", L], None, None, 1),
        ("tail -n 1M (stdin)", "tail", ["-n", "1000000"], L, None, 1),
        ("tac 100M", "tac", [L], None, None, 1),
        ("tr a-z A-Z 100M", "tr", ["a-z", "A-Z"], L, None, 1),
        ("tr -d digits 100M", "tr", ["-d", "0-9"], L, None, 1),
        ("cut -d, -f2,5 20M", "cut", ["-d,", "-f2,5", C], None, None, 1),
        ("cut -c1-10 100M", "cut", ["-c1-10", L], None, None, 1),
        ("uniq 1M lines", "uniq", [D], None, None, 1),
        ("uniq -c 1M lines", "uniq", ["-c", D], None, None, 1),
        ("nl 100M", "nl", [L], None, None, 1),
        ("paste -s 20M", "paste", ["-s", "-d,", N], None, None, 1),
        ("sort 100M", "sort", [L], None, None, 1),
        ("sort -n 20M", "sort", ["-n", N], None, None, 1),
        ("sort -k3,3 100M", "sort", ["-k3,3", L], None, None, 1),
        ("sort -u 100M", "sort", ["-u", L], None, None, 1),
        ("shuf 20M", "shuf", [N], None, None, 1),
        ("seq 10M", "seq", ["10000000"], None, None, 1),
        ("seq -w 0.5 step", "seq", ["-w", "0", "0.5", "2000000"], None, None, 1),
        # Checksums and encodings.
        ("md5sum 256M", "md5sum", [B], None, None, 1),
        ("sha256sum 256M", "sha256sum", [B], None, None, 1),
        ("b2sum 256M", "b2sum", [B], None, None, 1),
        ("base64 256M", "base64", [B], None, None, 1),
        ("base64 -d 256M", "base64", ["-d", p["blob.b64"]], None, None, 1),
        # Files and trees.
        ("cp 256M", "cp", [B, out], None, fresh(out), 1),
        ("cp -r 30k files", "cp", ["-r", T, out], None, fresh(out), 1),
        ("rm -r 30k files", "rm", ["-r", out], None, tree_copy, 1),
        ("ls -lR 30k files", "ls", ["-lR", T], None, None, 1),
        ("ls -1 300 dirs", "ls", ["-1", T + "/d000/s000"], None, None, 50),
        ("du -s 30k files", "du", ["-s", T], None, None, 1),
        ("du -ah 30k files", "du", ["-ah", T], None, None, 1),
        # Dogfish's -J records, against GNU's text for the same listing.
        ("ls -lR -J 30k files", "ls", {"gnu": ["-lR", T], "dogfish": ["-lR", "-J", T]}, None, None, 1),
        ("du -a -J 30k files", "du", {"gnu": ["-a", T], "dogfish": ["-a", "-J", T]}, None, None, 1),
        # An action record per file, against GNU's -v line per file.
        ("cp -r -J 30k files", "cp", {"gnu": ["-rv", T, out], "dogfish": ["-rJ", T, out]},
         None, fresh(out), 1),
        ("rm -r -J 30k files", "rm", {"gnu": ["-rv", out], "dogfish": ["-rJ", out]},
         None, tree_copy, 1),
        ("chmod -R 30k files", "chmod", ["-R", "u+w", T], None, None, 1),
        ("stat 100 files", "stat", [os.path.join(T, "d000/s000/f%03d.txt" % i) for i in range(100)], None, None, 1),
    ]


def run_once(argv, stdin, invocations, env, stdout_path):
    """Runs argv `invocations` times; returns (wall seconds, cpu seconds)."""
    cpu = 0.0
    start = time.perf_counter()
    for _ in range(invocations):
        fin = open(stdin, "rb") if stdin else subprocess.DEVNULL
        fout = open(stdout_path, "wb")
        proc = subprocess.Popen(argv, stdin=fin, stdout=fout,
                                stderr=subprocess.PIPE, env=env)
        fout.close()
        _, status, ru = os.wait4(proc.pid, 0)
        proc.returncode = os.waitstatus_to_exitcode(status)
        err = proc.stderr.read()
        proc.stderr.close()
        if fin is not subprocess.DEVNULL:
            fin.close()
        if proc.returncode != 0 and argv[0].split("/")[-1] not in ("false",):
            raise RuntimeError(f"{' '.join(argv)} exited {proc.returncode}: {err[:300]!r}")
        cpu += ru.ru_utime + ru.ru_stime
    return time.perf_counter() - start, cpu


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("-k", action="append", default=[], help="only benchmarks whose name contains this")
    ap.add_argument("--json", help="write results to this file")
    ap.add_argument("--baseline", help="compare against an earlier --json; exit 1 on a >15%% regression")
    ap.add_argument("--history", help="NDJSON file of earlier runs: compare, exit 1 on a regression, append")
    ap.add_argument("--self-test", action="store_true", help="test the regression check and exit")
    args = ap.parse_args()
    if args.self_test:
        return self_test()

    work = os.environ.get("WORK") or os.path.join(REPO, "target", "bench")
    repeat = int(os.environ.get("REPEAT", "7"))
    gnu_dir = os.environ.get("GNU", "/usr/bin")
    dogfish = os.environ.get("DOGFISH")
    if not dogfish:
        subprocess.run(["cargo", "build", "--release", "--locked", "--quiet"], cwd=REPO, check=True)
        dogfish = os.path.join(REPO, "target", "release", "dogfish")
    dogfish = os.path.realpath(dogfish)

    paths = gen_inputs(work)
    links = os.path.join(work, "bin")
    os.makedirs(links, exist_ok=True)
    scratch = os.path.join(work, "scratch")
    os.makedirs(scratch, exist_ok=True)
    env = dict(os.environ, LC_ALL="C", DOGFISH_UNDO="off", DOGFISH_HOME=os.path.join(work, "home"))
    env.pop("LANG", None)

    def measure(name, tool, argv, stdin, setup, inv, repeat):
        link = os.path.join(links, tool)
        if os.path.realpath(link) != dogfish:
            if os.path.lexists(link):
                os.remove(link)
            os.symlink(dogfish, link)
        # A dict gives each side its own arguments (for Dogfish-only options).
        per_side = argv if isinstance(argv, dict) else {"gnu": argv, "dogfish": argv}
        sides = {"gnu": [os.path.join(gnu_dir, tool)] + per_side["gnu"],
                 "dogfish": [link] + per_side["dogfish"]}
        samples = {"gnu": [], "dogfish": []}
        for i in range(repeat + 1):
            for side in (("gnu", "dogfish") if i % 2 else ("dogfish", "gnu")):
                if setup:
                    setup()
                w, c = run_once(sides[side], stdin, inv, env, os.path.join(scratch, "stdout"))
                if i:  # the first round warms caches
                    samples[side].append((w, c))
        med = {s: (statistics.median(x[0] for x in v), statistics.median(x[1] for x in v))
               for s, v in samples.items()}
        row = {"name": name, "gnu_wall": med["gnu"][0], "dogfish_wall": med["dogfish"][0],
               "gnu_cpu": med["gnu"][1], "dogfish_cpu": med["dogfish"][1]}
        row["wall_ratio"] = row["gnu_wall"] / max(row["dogfish_wall"], 1e-9)
        row["cpu_ratio"] = row["gnu_cpu"] / max(row["dogfish_cpu"], 1e-9)
        flag = "" if row["wall_ratio"] >= 1 and row["cpu_ratio"] >= 1 else "  <-- slower"
        print(f"{name:28} wall {row['gnu_wall']*1000:9.1f} ms GNU {row['dogfish_wall']*1000:9.1f} ms Dogfish "
              f"x{row['wall_ratio']:5.2f} | cpu {row['gnu_cpu']*1000:9.1f} / {row['dogfish_cpu']*1000:9.1f} "
              f"x{row['cpu_ratio']:5.2f}{flag}", flush=True)
        return row

    chosen = [b for b in benchmarks(paths, scratch) if not args.k or any(k in b[0] for k in args.k)]
    rows = [measure(*b, repeat) for b in chosen]

    def confirm(bad):
        """Measures each flagged benchmark again, twice as long; a regression
        stands only if the second measurement shows it too."""
        names = {b[0] for b in bad}
        print(f"\nmeasuring again: {', '.join(sorted(names))}")
        for b in chosen:
            if b[0] in names:
                row = measure(*b, 2 * repeat)
                rows[:] = [row if r["name"] == b[0] else r for r in rows]

    slower = [r["name"] for r in rows if r["wall_ratio"] < 1 or r["cpu_ratio"] < 1]
    print(f"\n{len(rows) - len(slower)}/{len(rows)} benchmarks: Dogfish at least as fast as GNU "
          f"in wall and CPU time." + (f" Slower: {', '.join(slower)}" if slower else ""))
    run = {"repeat": repeat, "cpu": cpu_model(), "commit": os.environ.get("GITHUB_SHA", ""),
           "date": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
           "results": rows}
    status = 0
    if args.baseline:
        baseline = [json.load(open(args.baseline))]
        bad = regressions(rows, baseline)
        if bad:
            confirm(bad)
            bad = regressions(rows, baseline)
        if bad:
            report(bad, "the baseline")
            status = 1
    if args.history:
        history = load_history(args.history, run["cpu"])
        bad = regressions(rows, history)
        if bad:
            confirm(bad)
            bad = regressions(rows, history)
        if bad:
            report(bad, f"the median of the last {min(len(history), HISTORY)} runs on this CPU")
            status = 1
        elif history:
            print(f"no regressions against the last {min(len(history), HISTORY)} runs on this CPU")
        else:
            print("no earlier runs on this CPU to compare with")
        with open(args.history, "a") as f:
            f.write(json.dumps(run, separators=(",", ":")) + "\n")
    if args.json:  # after any measurement again
        with open(args.json, "w") as f:
            json.dump(run, f, indent=1)
    return status


# A benchmark regresses when its GNU/Dogfish ratio, wall or CPU, falls more than
# THRESHOLD below the median of the last HISTORY comparable runs. Ratios rather
# than times, because GNU runs on the same machine in the same minute: a slower
# runner slows both sides, a slower Dogfish only one. 15% sits above the noise
# of shared runners on ratios and still catches a 20% slowdown (1/1.2 = 0.83).
THRESHOLD = 0.15
HISTORY = 10


def cpu_model():
    try:
        with open("/proc/cpuinfo") as f:
            for line in f:
                if line.startswith("model name"):
                    return line.split(":", 1)[1].strip()
    except OSError:
        pass
    return platform.processor() or platform.machine()


def load_history(path, cpu):
    """The runs in an NDJSON history file made on this CPU, oldest first."""
    runs = []
    if os.path.exists(path):
        with open(path) as f:
            for line in f:
                line = line.strip()
                if line:
                    run = json.loads(line)
                    if run.get("cpu") == cpu:
                        runs.append(run)
    return runs


def regressions(rows, history):
    """(name, metric, ratio, reference) for each ratio more than THRESHOLD below
    the median of the same benchmark's ratio over the last HISTORY runs."""
    bad = []
    recent = history[-HISTORY:]
    for row in rows:
        for metric in ("wall_ratio", "cpu_ratio"):
            past = [r[metric] for run in recent for r in run["results"]
                    if r["name"] == row["name"] and metric in r]
            if not past:
                continue
            ref = statistics.median(past)
            if row[metric] < (1 - THRESHOLD) * ref:
                bad.append((row["name"], metric, row[metric], ref))
    return bad


def report(bad, against):
    print(f"regressions (more than {THRESHOLD:.0%} worse than {against}):")
    for name, metric, ratio, ref in bad:
        print(f"  {name:28} {metric:10} x{ratio:5.2f}, was x{ref:5.2f}")


def self_test():
    """The regression test against synthetic runs: run-to-run noise of a few
    percent passes, a Dogfish 20% slower on one benchmark is caught."""
    rng = random.Random(7)

    def run(slow=None, factor=1.0):
        rows = []
        for i, name in enumerate(("cat", "sort", "wc -l", "startup: true")):
            g = 0.010 * (i + 1) * rng.uniform(0.97, 1.03)
            d = 0.008 * (i + 1) * rng.uniform(0.97, 1.03) * (factor if name == slow else 1)
            rows.append({"name": name, "wall_ratio": g / d, "cpu_ratio": g / d})
        return rows

    history = [{"results": run()} for _ in range(12)]
    assert regressions(run(), history) == [], "noise flagged as a regression"
    bad = regressions(run("sort", 1.2), history)
    assert {b[0] for b in bad} == {"sort"}, f"20% slowdown not caught: {bad}"
    assert regressions(run("sort", 1.2), []) == [], "flagged without history"
    print("self-test passed: noise passes, a 20% slowdown is caught")
    return 0


if __name__ == "__main__":
    sys.exit(main())
