#!/usr/bin/env python3
"""A scripted `lwk serve` session, checked against contract/serve.md.

    bench/serve_check.py [--lwk target/release/lwk] [--tree FOLDER] [--runs 200]
                         [--find QUERY]

What it does, on a large tree (FOLDER, read only; without one, microsoft/vscode
tag 1.105.0 is cloned shallow into a temp folder):

1. session: hello, then 20 interleaved find, grep and ls requests written at
   once, then a slow grep and its cancel, then shutdown. Checked: every id
   answered once, answers out of the order sent, the `cancelled` error before
   the cancel's own answer, exit code 0, one log line per request on stderr,
   and every result byte for byte the CLI's `--json` for the same params.
2. edges: a blank line, a line that is not JSON, one over 1 MB, an id still
   in flight, a cancel of an id not in flight, a folder out of scope, then
   stdin closed (exit 0).
3. cancel stops the work: the CPU a slow grep costs when left to end against
   when cancelled at once.
4. repeated find: the round trip of one query (--find) called again and again
   after a first call, median under 3 ms; the same for other shapes of query
   is printed, not checked. Also the first call of a fresh serve, with and
   without a cache file an earlier lwk left.
5. `--idle-exit 2`: exits 0 about 2 s after the last answer.

Stdlib only. LANEWORK_HOME is an empty temp folder (no app is asked) and
XDG_CACHE_HOME a temp folder (the user's own lwk cache is never touched).
Exit code 0 when every check passed, 1 otherwise.
"""

import argparse
import json
import os
import queue
import resource
import shutil
import statistics
import subprocess
import sys
import tempfile
import threading
import time

HERE = os.path.dirname(os.path.abspath(__file__))
SLOW_GREP = {"query": "zq[a-z]+xj[0-9]{3}qq", "regex": True}
WAIT = 60.0

failures = []


def check(ok, what):
    print(("  ok    " if ok else "  FAIL  ") + what)
    if not ok:
        failures.append(what)
    return ok


class Serve:
    """One `lwk serve`, its answers read on a thread as they come."""

    def __init__(self, lwk, tree, env, *args):
        self.log_file = tempfile.TemporaryFile()
        self.child = subprocess.Popen(
            [lwk, "serve", *args],
            cwd=tree,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.log_file,
        )
        self.lines = queue.Queue()
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.child.stdout:
            self.lines.put((time.perf_counter(), line))
        self.lines.put((time.perf_counter(), None))

    def send(self, *requests):
        """Write the requests (dicts, or raw bytes) in one write."""
        data = b"".join(
            r if isinstance(r, bytes) else json.dumps(r).encode() + b"\n" for r in requests
        )
        self.child.stdin.write(data)
        self.child.stdin.flush()

    def answer(self):
        """The next answer: (when, raw line, parsed); (when, None, None) at the end."""
        when, line = self.lines.get(timeout=WAIT)
        return (when, line, json.loads(line)) if line is not None else (when, None, None)

    def answers(self, count):
        return [self.answer() for _ in range(count)]

    def wait(self):
        return self.child.wait(timeout=WAIT)

    def logs(self):
        self.log_file.seek(0)
        return [json.loads(line) for line in self.log_file.read().splitlines() if line.strip()]


def raw_result(line, request_id):
    """The bytes of `result` in an answer line, as they were sent."""
    head = b'{"id":' + json.dumps(request_id).encode() + b',"result":'
    if not (line.startswith(head) and line.endswith(b"}\n")):
        return None
    return line[len(head):-2]


def cli(lwk, tree, env, method, params):
    """The CLI's `--json` stdout for the same params, run in `tree`."""
    params = dict(params)
    command = {"find_files": "find", "search_text": "grep", "list_files": "ls"}[method]
    argv = [lwk, command, "--json"]
    if "limit" in params:
        argv += ["--limit", str(params.pop("limit"))]
    if params.pop("git", False):
        argv.append("--git")
    if "regex" in params:
        argv.append("-E" if params.pop("regex") else "-F")
    if "case_sensitive" in params:
        argv.append("-s" if params.pop("case_sensitive") else "-i")
    for glob in params.pop("globs", []):
        argv += ["-g", glob]
    folder = params.pop("folder", None)
    argv.append("--")
    if command != "ls":
        argv.append(params.pop("query"))
    if folder is not None:
        argv.append(folder)
    assert not params, params
    done = subprocess.run(argv, cwd=tree, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    return done.stdout


def as_cli(method, raw):
    """A serve result as the CLI prints the tool's answer."""
    if method == "find_files":
        return b'{"files":' + raw + b"}\n"
    if method == "search_text":
        return b'{"matches":' + raw + b"}\n"
    return raw + b"\n"


def interleaved(tree):
    """20 requests: a broad grep first, then find, grep and ls in turn."""
    finds = [
        {"query": "extHostTypes"},
        {"query": "editr widget", "limit": 5},
        {"query": "package.json", "limit": 20, "git": True},
        {"query": "*.css button", "limit": 10},
        {"query": "terminal", "folder": "src/vs/workbench", "limit": 200},
        {"query": "readme", "folder": "extensions"},
        {"query": "zzzzqqqqxxxx"},
    ]
    greps = [
        {"query": "[A-Z][a-z]+Service\\b.*\\bdispose\\(", "regex": True, "limit": 200},
        {"query": "TODO", "limit": 10},
        {"query": "IDisposable", "case_sensitive": True, "limit": 30, "globs": ["*.ts", "!*.test.ts"]},
        {"query": "copyright", "folder": "build", "limit": 5},
        {"query": "^import .* from 'vs/base", "regex": True, "limit": 3},
        {"query": "no such text anywhere zq9", "folder": "extensions"},
        {"query": "function", "globs": ["*.md"], "limit": 4},
    ]
    lists = [
        {},
        {"limit": 5},
        {"folder": "src/vs/base", "limit": 50000},
        {"folder": "extensions", "limit": 1},
        {"limit": 50000},
        {"folder": "build/lib"},
    ]
    order = [("search_text", greps[0])]
    rest = [("find_files", f) for f in finds] + [("search_text", g) for g in greps[1:]]
    rest += [("list_files", l) for l in lists]
    # f, g, l, f, g, l, ...
    by_kind = {"find_files": [], "search_text": [], "list_files": []}
    for method, params in rest:
        by_kind[method].append((method, params))
    while any(by_kind.values()):
        for kind in ("find_files", "search_text", "list_files"):
            if by_kind[kind]:
                order.append(by_kind[kind].pop(0))
    assert len(order) == 20, len(order)
    return [(n + 1 if n % 2 else "r%d" % (n + 1), m, p) for n, (m, p) in enumerate(order)]


def session(lwk, tree, env):
    print("1. session: hello, 20 interleaved calls, a cancel, shutdown")
    serve = Serve(lwk, tree, env)
    serve.send({"id": "hi", "method": "hello"})
    _, _, hello = serve.answer()
    version = json.loads(subprocess.run([lwk, "version", "--json"], stdout=subprocess.PIPE).stdout)
    want = {"lwk": version["lwk"], "contract": version["contract"], "pid": serve.child.pid}
    check(hello == {"id": "hi", "result": want}, "hello answers %s" % json.dumps(want))

    calls = interleaved(tree)
    serve.send(*[
        {"id": i, "method": m, "params": dict(p, cwd=tree, roots=[tree])} for i, m, p in calls
    ])
    got = serve.answers(len(calls))
    sent = [i for i, _, _ in calls]
    came = [parsed["id"] for _, _, parsed in got]
    check(sorted(map(str, came)) == sorted(map(str, sent)), "20 ids answered, each once")
    check(came != sent, "answers came out of the order sent (%s ...)" % json.dumps(came[:6]))
    check(came.index(sent[0]) > 0, "the broad grep sent first did not answer first")
    by_id = {json.dumps(parsed["id"]): (line, parsed) for _, line, parsed in got}
    same = 0
    for request_id, method, params in calls:
        line, parsed = by_id.get(json.dumps(request_id), (b"", {}))
        raw = raw_result(line, request_id)
        if raw is None:
            check(False, "%s %s answered a result: %s" % (method, json.dumps(params), line[:200]))
            continue
        if as_cli(method, raw) == cli(lwk, tree, env, method, params):
            same += 1
        else:
            check(False, "%s %s is the CLI's --json, byte for byte" % (method, json.dumps(params)))
    check(same == len(calls), "%d of %d results byte-identical to the CLI's --json" % (same, len(calls)))
    empty = [parsed for _, parsed in by_id.values() if parsed.get("result") in ([], None)]
    check(len(empty) < 4, "the results are not empty ones (%d of 20 are)" % len(empty))

    slow = dict(SLOW_GREP, cwd=tree)
    serve.send(
        {"id": "slow", "method": "search_text", "params": slow},
        {"id": "stop", "method": "cancel", "params": {"id": "slow"}},
    )
    (_, _, first), (_, _, second) = serve.answers(2)
    cancelled = {"id": "slow", "error": {"code": "cancelled", "message": "Cancelled."}}
    check(first == cancelled, "the cancelled grep answers %s" % json.dumps(first))
    check(second == {"id": "stop", "result": {"cancelled": True}}, "cancel answers cancelled: true, after it")
    serve.send({"id": "again", "method": "cancel", "params": {"id": "slow"}})
    _, _, again = serve.answer()
    check(again == {"id": "again", "result": {"cancelled": False}}, "a second cancel answers cancelled: false")

    serve.send({"id": 99, "method": "shutdown"})
    _, line, _ = serve.answer()
    check(line == b'{"id":99,"result":null}\n', "shutdown answers null")
    check(serve.answer()[1] is None, "nothing is written after it")
    check(serve.wait() == 0, "exit code 0")

    logs = serve.logs()
    keyed = all(
        isinstance(log.get("method"), str)
        and isinstance(log.get("ms"), (int, float))
        and (log.get("ok") is True) != isinstance(log.get("error"), str)
        for log in logs
    )
    check(len(logs) == 25 and keyed, "25 log lines on stderr, each with method, ms, and ok or error")
    codes = {json.dumps(log["id"]): log.get("error") for log in logs}
    check(codes.get('"slow"') == "cancelled", 'the cancelled call is logged "error": "cancelled"')


def edges(lwk, tree, env):
    print("2. edges")
    serve = Serve(lwk, tree, env)
    serve.send(b"\n", b"   \n", b"not json\n", b"[1]\n", b'{"method":"hello"}\n')
    for what in ("a line that is not JSON", "a line that is not an object", "a request with no id"):
        _, _, parsed = serve.answer()
        ok = parsed["id"] is None and parsed["error"]["code"] == "invalid"
        check(ok, "%s is invalid, id null (blank lines ignored)" % what)
    serve.send(b'{"id":1,"method":"hello","pad":"' + b"x" * (1024 * 1024) + b'"}\n')
    _, _, parsed = serve.answer()
    check(parsed["id"] is None and parsed["error"]["code"] == "invalid", "a line over 1 MB is invalid, id null")
    serve.send({"id": 2, "method": "nope"}, {"id": 3, "method": "find_files", "params": {"query": "x"}})
    _, _, unknown = serve.answer()
    check(unknown == {"id": 2, "error": {"code": "invalid", "message": "Unknown method: nope"}}, "an unknown method is invalid")
    _, _, parsed = serve.answer()
    check(parsed["id"] == 3 and parsed["error"]["code"] == "invalid", "a call with no cwd is invalid")
    serve.send(
        {"id": 4, "method": "list_files", "params": {"folder": "/etc", "cwd": tree}},
        {"id": 5, "method": "list_files", "params": {"cwd": tree, "roots": [os.path.join(tree, "src")]}},
        {"id": 6, "method": "list_files", "params": {"folder": "no-such-folder", "cwd": tree}},
    )
    codes = {parsed["id"]: parsed["error"]["code"] for _, _, parsed in serve.answers(3)}
    check(codes == {4: "scope", 5: "scope", 6: "not_found"}, "folders out of the roots are scope, a missing one not_found")
    slow = dict(SLOW_GREP, cwd=tree)
    serve.send(
        {"id": "dup", "method": "search_text", "params": slow},
        {"id": "dup", "method": "hello"},
        {"id": 7, "method": "cancel", "params": {}},
        {"id": 8, "method": "cancel", "params": {"id": "never sent"}},
    )
    got = [parsed for _, _, parsed in serve.answers(3)]
    check(got[0]["id"] == "dup" and got[0].get("error", {}).get("code") == "invalid", "an id still in flight is refused invalid")
    check(got[1]["id"] == 7 and got[1].get("error", {}).get("code") == "invalid", "a cancel with no id is invalid")
    check(got[2] == {"id": 8, "result": {"cancelled": False}}, "a cancel of an id never sent answers cancelled: false")
    serve.child.stdin.close()
    check(serve.answer()[1] is None, "stdin closed: the call in flight answers nothing")
    check(serve.wait() == 0, "stdin closed: exit code 0")


def cpu_of(lwk, tree, env, cancel, pause):
    """The CPU seconds a serve spent on one slow grep, cancelled at once or not."""
    before = resource.getrusage(resource.RUSAGE_CHILDREN)
    serve = Serve(lwk, tree, env)
    started = time.perf_counter()
    request = {"id": 1, "method": "search_text", "params": dict(SLOW_GREP, cwd=tree)}
    if cancel:
        serve.send(request, {"id": 2, "method": "cancel", "params": {"id": 1}})
        serve.answers(2)
        # Were the work not stopped, it would go on through this pause.
        time.sleep(pause)
    else:
        serve.send(request)
        serve.answer()
    took = time.perf_counter() - started
    serve.send({"id": 3, "method": "shutdown"})
    serve.wait()
    after = resource.getrusage(resource.RUSAGE_CHILDREN)
    cpu = (after.ru_utime + after.ru_stime) - (before.ru_utime + before.ru_stime)
    return cpu, took


def cancel_stops_work(lwk, tree, env):
    print("3. cancel stops the work")
    whole, took = cpu_of(lwk, tree, env, False, 0)
    cut, _ = cpu_of(lwk, tree, env, True, took * 2 + 0.2)
    print("     slow grep left to end: %.0f ms, %.0f ms CPU; cancelled at once: %.0f ms CPU" % (took * 1e3, whole * 1e3, cut * 1e3))
    check(cut < whole / 2, "a cancelled grep costs under half the CPU of a whole one")


def find_times(serve, tree, query, runs, first_id):
    """Round trips (ms) of `runs` find calls for one query, one at a time."""
    times = []
    for n in range(runs):
        params = {"query": query, "cwd": tree}
        sent = time.perf_counter()
        serve.send({"id": first_id + n, "method": "find_files", "params": params})
        when, _, parsed = serve.answer()
        assert parsed["id"] == first_id + n and "result" in parsed, parsed
        times.append((when - sent) * 1e3)
    return times


def first_find(lwk, tree, env, query):
    """The first find of a fresh serve: its round trip and lwk's own time."""
    serve = Serve(lwk, tree, env)
    took = find_times(serve, tree, query, 1, 1)[0]
    serve.send({"id": "end", "method": "shutdown"})
    serve.wait()
    return took, serve.logs()[0]["ms"]


def repeated_find(lwk, tree, env, runs, query):
    print("4. repeated find (%d calls each, after a first)" % runs)
    cold_env = dict(env, XDG_CACHE_HOME=os.path.join(env["XDG_CACHE_HOME"], "cold"))
    os.mkdir(cold_env["XDG_CACHE_HOME"])
    cold = first_find(lwk, tree, cold_env, query)
    warm = first_find(lwk, tree, env, query)
    print("     first call of a fresh serve: %.2f ms with no cache file (%.2f in lwk), "
          "%.2f ms with an earlier lwk's (%.2f in lwk)" % (cold[0], cold[1], warm[0], warm[1]))

    # The query the check is on, then other shapes of query for the record.
    others = [q for q in ("package.json", "editr widget", "terminal service", "main") if q != query]
    serve = Serve(lwk, tree, env)
    find_times(serve, tree, query, 1, 0)
    rows = []
    for at, q in enumerate([query] + others):
        find_times(serve, tree, q, 5, 10_000 * (at + 1))
        rows.append((q, sorted(find_times(serve, tree, q, runs, 10_000 * (at + 1) + 5))))
    serve.send({"id": "end", "method": "shutdown"})
    serve.wait()
    served = {}
    for log in serve.logs():
        if log["method"] == "find_files" and log["id"] >= 10_000 and log["id"] % 10_000 >= 5:
            served.setdefault(log["id"] // 10_000 - 1, []).append(log["ms"])
    print("     %-20s %8s %8s %8s %8s   %s" % ("query", "median", "p90", "min", "max", "median in lwk (its log)"))
    for at, (q, times) in enumerate(rows):
        median = statistics.median(times)
        print("     %-20s %8.2f %8.2f %8.2f %8.2f   %.2f ms%s" % (
            json.dumps(q), median, times[int(len(times) * 0.9)], times[0], times[-1],
            statistics.median(served[at]), "" if median < 3.0 else "   (over 3 ms)"))
    median = statistics.median(rows[0][1])
    check(median < 3.0, "find %s repeated: median round trip under 3 ms (%.2f ms)" % (json.dumps(query), median))


def idle_exit(lwk, tree, env):
    print("5. --idle-exit 2")
    serve = Serve(lwk, tree, env, "--idle-exit", "2")
    serve.send({"id": 1, "method": "find_files", "params": {"query": "main", "cwd": tree}})
    last, _, parsed = serve.answer()
    code = serve.wait()
    ended, line, _ = serve.answer()
    waited = time.perf_counter() - last
    print("     exited %.2f s after the last answer" % waited)
    check(parsed["id"] == 1 and "result" in parsed and line is None, "it answered, then wrote nothing more")
    check(code == 0 and 1.9 <= waited <= 2.6, "exit code 0, about 2 s after the last answer (%.2f s)" % waited)


def main():
    parser = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    parser.add_argument("--lwk", default=os.path.join(HERE, "..", "target", "release", "lwk"))
    parser.add_argument("--tree", help="a large folder to search, read only (default: clone vscode 1.105.0)")
    parser.add_argument("--runs", type=int, default=200)
    parser.add_argument("--find", default="extHostTypes", help="the query of the repeated find that is checked")
    args = parser.parse_args()
    lwk = os.path.realpath(args.lwk)
    if not os.access(lwk, os.X_OK):
        sys.exit("serve_check: no lwk at %s (cargo build --release)" % lwk)

    scratch = tempfile.mkdtemp(prefix="lwk-serve-check-")
    try:
        env = dict(os.environ)
        env["LANEWORK_HOME"] = os.path.join(scratch, "lanework-home")
        env["XDG_CACHE_HOME"] = os.path.join(scratch, "xdg-cache")
        env.pop("LWK_NO_CACHE", None)
        os.mkdir(env["LANEWORK_HOME"])
        os.mkdir(env["XDG_CACHE_HOME"])
        tree = args.tree
        if tree is None:
            tree = os.path.join(scratch, "vscode")
            print("cloning microsoft/vscode 1.105.0 into %s" % tree)
            subprocess.run(
                ["git", "clone", "--quiet", "--depth", "1", "--branch", "1.105.0",
                 "https://github.com/microsoft/vscode", tree],
                check=True,
            )
        tree = os.path.realpath(tree)
        print("lwk  %s\ntree %s" % (lwk, tree))
        session(lwk, tree, env)
        edges(lwk, tree, env)
        cancel_stops_work(lwk, tree, env)
        repeated_find(lwk, tree, env, args.runs, args.find)
        idle_exit(lwk, tree, env)
    finally:
        shutil.rmtree(scratch, ignore_errors=True)
    if failures:
        print("\n%d check(s) failed:" % len(failures))
        for what in failures:
            print("  - " + what)
        return 1
    print("\nall checks passed")
    return 0


if __name__ == "__main__":
    sys.exit(main())
