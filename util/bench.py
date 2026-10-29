#!/usr/bin/env python3
"""Head-to-head benchmarks: Dogfish against GNU coreutils on the same inputs.

  util/bench.py                      build Dogfish, run every benchmark, print a table
  util/bench.py -k sort -k wc        only benchmarks whose name contains a pattern
  util/bench.py --json out.json      also write the results
  util/bench.py --baseline b.json    fail if a ratio is more than 20% worse than in b.json

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
import json
import os
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
    """(name, tool, argv-after-tool, stdin path or None, setup per run or None, invocations)."""
    L, N, C, D, B = p["log.txt"], p["nums.txt"], p["data.csv"], p["dups.txt"], p["blob.bin"]
    T = p["tree"]
    out = os.path.join(scratch, "out")

    def fresh(path):
        return lambda: shutil.rmtree(path, ignore_errors=True) if os.path.isdir(path) else (
            os.path.exists(path) and os.remove(path))

    def tree_copy():
        shutil.rmtree(out, ignore_errors=True)
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
    ap.add_argument("--baseline", help="compare against an earlier --json; exit 1 on a >20%% regression")
    args = ap.parse_args()

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

    rows = []
    for name, tool, argv, stdin, setup, inv in benchmarks(paths, scratch):
        if args.k and not any(k in name for k in args.k):
            continue
        link = os.path.join(links, tool)
        if os.path.realpath(link) != dogfish:
            if os.path.lexists(link):
                os.remove(link)
            os.symlink(dogfish, link)
        sides = {"gnu": [os.path.join(gnu_dir, tool)] + argv, "dogfish": [link] + argv}
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
        rows.append(row)
        flag = "" if row["wall_ratio"] >= 1 and row["cpu_ratio"] >= 1 else "  <-- slower"
        print(f"{name:28} wall {row['gnu_wall']*1000:9.1f} ms GNU {row['dogfish_wall']*1000:9.1f} ms Dogfish "
              f"x{row['wall_ratio']:5.2f} | cpu {row['gnu_cpu']*1000:9.1f} / {row['dogfish_cpu']*1000:9.1f} "
              f"x{row['cpu_ratio']:5.2f}{flag}", flush=True)

    slower = [r["name"] for r in rows if r["wall_ratio"] < 1 or r["cpu_ratio"] < 1]
    print(f"\n{len(rows) - len(slower)}/{len(rows)} benchmarks: Dogfish at least as fast as GNU "
          f"in wall and CPU time." + (f" Slower: {', '.join(slower)}" if slower else ""))
    if args.json:
        with open(args.json, "w") as f:
            json.dump({"repeat": repeat, "results": rows}, f, indent=1)
    if args.baseline:
        base = {r["name"]: r for r in json.load(open(args.baseline))["results"]}
        bad = [r["name"] for r in rows if r["name"] in base
               and r["wall_ratio"] < 0.8 * base[r["name"]]["wall_ratio"]]
        if bad:
            print("regressions (>20% worse than baseline): " + ", ".join(bad))
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
