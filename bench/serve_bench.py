#!/usr/bin/env python3
"""Repeated find_files and list_files through one `lwk serve`, for bench/run.sh.

    python3 -I bench/serve_bench.py --lwk target/release/lwk --tree FOLDER \
        --out serve-NAME.json --runs 200 \
        --find one=QUERY --find multi=QUERY ...

One `lwk serve` is started in FOLDER. Each query is sent once to warm it (that
first round trip is recorded too), then N times, one request at a time, and
the round trip of each (request written until its answer line is read) is
timed. The result is a JSON file with the median and p90 per query.

Environment (set by bench/run.sh, passed on as is): LANEWORK_HOME is an empty
folder, so no Lanework app is asked, and XDG_CACHE_HOME a folder under
bench/.corpus. Standard library only.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
import time


def p90(times):
    s = sorted(times)
    return s[min(len(s) - 1, int(round(0.9 * (len(s) - 1))))]


def count(result):
    """How many files an answer holds ({"files": [...]})."""
    return len(result["files"]) if isinstance(result, dict) and "files" in result else len(result)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lwk", required=True)
    ap.add_argument("--tree", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--runs", type=int, default=200)
    ap.add_argument("--find", action="append", default=[], metavar="LABEL=QUERY")
    ap.add_argument("--git", action="store_true", help="also time each find with git: true")
    args = ap.parse_args()
    for var in ("LANEWORK_HOME", "XDG_CACHE_HOME"):
        if not os.environ.get(var):
            sys.exit(var + " must be set (an empty folder / a folder under bench/.corpus)")

    tree = os.path.realpath(args.tree)
    cases = []
    for item in args.find:
        label, _, query = item.partition("=")
        cases.append(("find " + label, "find_files", {"query": query}, query))
        if args.git:
            cases.append(("find " + label + " --git", "find_files", {"query": query, "git": True}, query))
    cases.append(("ls", "list_files", {}, None))
    cases.append(("ls all", "list_files", {"limit": 50000}, None))

    child = subprocess.Popen(
        [args.lwk, "serve"], cwd=tree, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    next_id = [0]

    def call(method, params):
        next_id[0] += 1
        req = {"id": next_id[0], "method": method, "params": dict(params, cwd=tree, roots=[tree])}
        t0 = time.perf_counter()
        child.stdin.write(json.dumps(req).encode() + b"\n")
        child.stdin.flush()
        line = child.stdout.readline()
        took = time.perf_counter() - t0
        ans = json.loads(line)
        if ans.get("id") != req["id"] or "result" not in ans:
            sys.exit("bad answer to %s: %s" % (method, line[:200]))
        return took, ans["result"]

    results = []
    try:
        call("hello", {})
        for name, method, params, query in cases:
            first, res = call(method, params)
            times = [call(method, params)[0] for _ in range(args.runs)]
            results.append({
                "name": name, "method": method, "query": query,
                "results": count(res), "runs": args.runs,
                "first": first, "median": statistics.median(times),
                "p90": p90(times), "min": min(times),
            })
            print("   %-22s median %.2f ms, p90 %.2f ms (%d results)"
                  % (name, results[-1]["median"] * 1000, results[-1]["p90"] * 1000, count(res)))
    finally:
        try:
            child.stdin.close()
        except OSError:
            pass
        child.wait(timeout=30)
    with open(args.out, "w") as f:
        json.dump({"tree": tree, "results": results}, f, indent=1)


if __name__ == "__main__":
    main()
