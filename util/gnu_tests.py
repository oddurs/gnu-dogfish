#!/usr/bin/env python3
"""Select and report GNU coreutils tests for util/gnu-tests.sh.

  gnu_tests.py select SRC TOOLS_FILE OUT_JSON   choose tests that only exercise Dogfish tools
  gnu_tests.py names OUT_JSON                   print the chosen test paths, space-separated
  gnu_tests.py report SRC SELECTION_JSON RESULTS_JSON [--summary MD]
                      [--baseline JSON] [--update-baseline]

A test counts when every program it declares with `print_ver_` (shell tests)
or `$prog` (Perl tests) is one Dogfish provides. Results come from the
automake `.trs` files. With a baseline, a test that passed there and does
not pass now is a regression and the report exits 1.
"""

import json
import os
import re
import sys
from collections import Counter, defaultdict

RESULTS = ("PASS", "FAIL", "SKIP", "XFAIL", "XPASS", "ERROR")


def all_tests(src):
    """Test paths listed in `all_tests` in the generated Makefile.in."""
    text = open(os.path.join(src, "Makefile.in"), encoding="utf-8", errors="replace").read()
    block = re.search(r"^all_tests = \\\n(.*?)(?:\n\s*\n|\n[^\t ])", text, re.S | re.M)
    if not block:
        sys.exit("gnu_tests: no all_tests in Makefile.in")
    return re.findall(r"tests/\S+\.(?:sh|pl|xpl)", block.group(1))


def tools_under_test(src, test, known):
    """Programs a test exercises, from its own declaration where it has one."""
    body = open(os.path.join(src, test), encoding="utf-8", errors="replace").read()
    if m := re.search(r"^[^#\n]*\bprint_ver_\s+([^\n;&|#]+)", body, re.M):
        return m.group(1).split()
    if m := re.search(r"\$prog\s*=\s*['\"]([^'\"]+)['\"]", body):
        return [m.group(1)]
    parts = test.split("/")
    if len(parts) >= 3 and parts[1] != "misc":
        return [parts[1]]
    stem = os.path.splitext(parts[-1])[0]
    for candidate in (stem, stem.split("-")[0]):
        if candidate in known:
            return [candidate]
    return []


def cmd_select(src, tools_file, out):
    dogfish = {line.strip() for line in open(tools_file) if line.strip()}
    built = set(os.listdir(os.path.join(src, "src")))
    chosen, skipped = {}, Counter()
    for test in all_tests(src):
        tools = tools_under_test(src, test, dogfish | built)
        if tools and all(t in dogfish for t in tools):
            chosen[test] = tools
        else:
            skipped["not Dogfish" if tools else "unknown program"] += 1
    json.dump({"tests": chosen}, open(out, "w"), indent=1, sort_keys=True)
    print(f"gnu_tests: selected {len(chosen)} tests "
          f"({skipped['not Dogfish']} exercise programs Dogfish lacks, "
          f"{skipped['unknown program']} declare no program)")


def cmd_names(selection):
    print(" ".join(sorted(json.load(open(selection))["tests"])))


def result_of(src, test):
    trs = os.path.join(src, os.path.splitext(test)[0] + ".trs")
    try:
        text = open(trs, encoding="utf-8", errors="replace").read()
    except FileNotFoundError:
        return "ERROR"
    found = re.findall(r"^:test-result:\s*(\w+)", text, re.M)
    if not found:
        return "ERROR"
    return next((r for r in ("ERROR", "FAIL", "XPASS") if r in found), found[0])


def cmd_report(argv):
    src, selection, out = argv[:3]
    opts = argv[3:]

    def opt(name):
        return opts[opts.index(name) + 1] if name in opts else None

    tests = json.load(open(selection))["tests"]
    results = {t: result_of(src, t) for t in sorted(tests)}
    totals = Counter(results.values())
    per_tool = defaultdict(Counter)
    for test, result in results.items():
        for tool in tests[test]:
            per_tool[tool][result] += 1
    ran = sum(totals[r] for r in RESULTS if r != "SKIP")
    passed = totals["PASS"] + totals["XFAIL"]
    rate = 100.0 * passed / ran if ran else 0.0
    json.dump({"totals": dict(totals), "pass_rate": round(rate, 1),
               "tests": {t: {"result": r, "tools": tests[t]} for t, r in results.items()}},
              open(out, "w"), indent=1, sort_keys=True)

    baseline_path = opt("--baseline")
    regressions, improvements = [], []
    baseline = None
    if baseline_path and os.path.exists(baseline_path):
        baseline = set(json.load(open(baseline_path))["pass"])
        now = {t for t, r in results.items() if r in ("PASS", "XFAIL")}
        regressions = sorted(t for t in baseline if t in results and t not in now)
        improvements = sorted(now - baseline)

    lines = [
        f"## GNU coreutils test suite: {passed}/{ran} passing ({rate:.1f}%)",
        "",
        "| Result | Tests |",
        "| --- | ---: |",
        *(f"| {r} | {totals[r]} |" for r in RESULTS if totals[r]),
        "",
        "| Program | Pass | Fail | Error | Skip |",
        "| --- | ---: | ---: | ---: | ---: |",
        *(f"| `{tool}` | {c['PASS'] + c['XFAIL']} | {c['FAIL'] + c['XPASS']} | {c['ERROR']} | {c['SKIP']} |"
          for tool, c in sorted(per_tool.items())),
    ]
    failing = [t for t, r in results.items() if r in ("FAIL", "ERROR", "XPASS")]
    if failing:
        lines += ["", "<details><summary>Failing tests</summary>", "",
                  *(f"- `{t}` ({results[t]})" for t in failing), "", "</details>"]
    if baseline is not None:
        lines += ["", f"Against the baseline: {len(regressions)} regressions, {len(improvements)} newly passing."]
        lines += [f"- regression: `{t}`" for t in regressions]
    summary = "\n".join(lines) + "\n"
    print(summary)
    if path := opt("--summary"):
        open(path, "w").write(summary)
    if path := os.environ.get("GITHUB_STEP_SUMMARY"):
        open(path, "a").write(summary)

    if "--update-baseline" in opts and baseline_path:
        passing = sorted(t for t, r in results.items() if r in ("PASS", "XFAIL"))
        json.dump({"gnu_version": os.path.basename(os.path.abspath(src)), "pass": passing},
                  open(baseline_path, "w"), indent=1)
        print(f"gnu_tests: baseline updated ({len(passing)} passing)")
        return 0
    return 1 if regressions else 0


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    cmd, argv = sys.argv[1], sys.argv[2:]
    if cmd == "select" and len(argv) == 3:
        cmd_select(*argv)
    elif cmd == "names" and len(argv) == 1:
        cmd_names(*argv)
    elif cmd == "report" and len(argv) >= 3:
        sys.exit(cmd_report(argv))
    else:
        sys.exit(__doc__)


if __name__ == "__main__":
    main()
