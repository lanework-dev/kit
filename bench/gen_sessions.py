#!/usr/bin/env python3
"""Write a synthetic transcript home for the `sessions` benchmark.

    python3 -I bench/gen_sessions.py OUT_HOME CORPUS_DIR [--mb N]

OUT_HOME becomes a fake $HOME holding, in the formats `lwk sessions` reads
(src/engine/sessions/):

  .claude/projects/<folder>/<uuid>.jsonl        Claude Code
  .codex/sessions/Y/M/D/rollout-<ts>-<uuid>.jsonl, .codex/session_index.jsonl
  .local/share/opencode/opencode.db             OpenCode (session_v2 +
                                                session_message, and the
                                                older session + part)

Most sessions ran in CORPUS_DIR or a folder inside it, so a search from the
corpus is scoped to them; the rest ran elsewhere (including a sibling folder
whose name starts with the corpus folder's) and must be left out.

Standard library only; fixed seed, fixed clock: the same arguments write the
same bytes (the file times are set, so "newest first" is stable too).
"""

import argparse
import json
import os
import random
import re
import shutil
import sqlite3
import sys

SEED = 20261118
BASE_MS = 1_794_000_000_000  # a fixed "now"; session times count back from it
DAY_MS = 86_400_000

WORDS = (
    "the a of to and in is it that for on with as this be by at from or an "
    "component state props render hook effect cleanup dependency array "
    "request response handler route server client cache index query result "
    "error retry timeout stream buffer parse token session user folder file "
    "test fixture mock snapshot build bundle lint format type interface "
    "refactor rename extract move delete add remove update fix check run "
    "should would could need want maybe because instead before after still "
    "already always never every each some many few more less than then when "
    "where which while until unless between inside outside across around"
).split()

SNIPPETS = [
    "useEffect(() => {\n  load();\n  return () => cancel();\n}, []);",
    "useEffect(() => { setReady(true); }, [props.id]);",
    "const [state, setState] = useState(null);",
    "export function useThing(id: string) { return useMemo(() => build(id), [id]); }",
    "setTimeout(() => flush(), 0);",
    "registerCommand('workbench.action.reload', handler);",
    "fn main() {\n    let args: Vec<String> = std::env::args().collect();\n}",
    "def handler(request):\n    return Response(status=200)",
    "SELECT id, title FROM session WHERE directory = ?;",
    "import { readFileSync } from 'node:fs';",
]

TOOLS = ["Read", "Grep", "Glob", "Edit", "Bash", "Write"]
SUBDIRS = ["", "", "", "apps/web", "apps/desktop", "packages/ui", "packages/protocol", "docs", "scripts"]
OTHER_FOLDERS = [
    "/Users/dev/projects/billing-service",
    "/Users/dev/projects/mobile-app",
    "/Users/dev/notes",
    "/Users/dev/projects/website/apps/blog",
]


class Gen:
    def __init__(self, seed):
        self.r = random.Random(seed)
        # Reused paragraphs: building thousands of fresh ones would dominate
        # the run. About one in 25 mentions useEffect, so a search for it is
        # neither empty nor everywhere.
        self.pool = [self._paragraph() for _ in range(1500)]

    def uuid(self):
        h = "%032x" % self.r.getrandbits(128)
        return f"{h[:8]}-{h[8:12]}-4{h[13:16]}-8{h[17:20]}-{h[20:32]}"

    def _paragraph(self):
        r = self.r
        words = [r.choice(WORDS) for _ in range(r.randint(25, 140))]
        text = " ".join(words)
        if r.random() < 0.04:
            text += "\n\n```tsx\n" + SNIPPETS[0 if r.random() < 0.6 else 1] + "\n```"
        elif r.random() < 0.15:
            text += "\n\n```\n" + r.choice(SNIPPETS[2:]) + "\n```"
        return text

    def text(self, paragraphs=1):
        return "\n\n".join(self.r.choice(self.pool) for _ in range(paragraphs))

    def tool_output(self):
        # Long, like a file read or a build log.
        r = self.r
        return "\n".join(
            f"{n:4d}\t{r.choice(self.pool)[: r.randint(20, 160)]}" for n in range(1, r.randint(20, 120))
        )

    def title(self):
        return " ".join(self.r.choice(WORDS) for _ in range(self.r.randint(3, 7))).capitalize()


def iso(ms):
    s, msr = divmod(ms, 1000)
    import time

    return time.strftime("%Y-%m-%dT%H:%M:%S", time.gmtime(s)) + f".{msr:03d}Z"


def claude_dir(cwd):
    return re.sub(r"[^A-Za-z0-9]", "-", cwd)


def write_lines(path, objs, mtime_ms):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as f:
        for o in objs:
            f.write(json.dumps(o, ensure_ascii=False, separators=(",", ":")))
            f.write("\n")
    t = mtime_ms / 1000
    os.utime(path, (t, t))
    return os.path.getsize(path)


def claude_session(g, home, cwd, sid, turns, mtime_ms):
    r = g.r
    parent = None
    rows = [{"type": "ai-title", "aiTitle": g.title(), "sessionId": sid}]
    t0 = mtime_ms - turns * 60_000
    for n in range(turns):
        ts = iso(t0 + n * 60_000)
        uid = g.uuid()
        rows.append({"type": "attachment", "cwd": cwd, "sessionId": sid, "timestamp": ts,
                     "attachment": {"type": "diagnostics", "files": [g.text(1)[:300]]}})
        rows.append({"parentUuid": parent, "isSidechain": False, "userType": "external", "cwd": cwd,
                     "sessionId": sid, "version": "2.1.327", "type": "user", "uuid": uid, "timestamp": ts,
                     "message": {"role": "user", "content": g.text(r.randint(1, 2))}})
        aid = g.uuid()
        tool_id = "toolu_" + g.uuid().replace("-", "")[:22]
        rows.append({"parentUuid": uid, "isSidechain": False, "cwd": cwd, "sessionId": sid,
                     "type": "assistant", "uuid": aid, "timestamp": ts,
                     "message": {"role": "assistant", "model": "claude-sonnet-5-5", "id": "msg_" + g.uuid()[:20],
                                 "content": [{"type": "text", "text": g.text(r.randint(1, 3))},
                                             {"type": "tool_use", "id": tool_id, "name": r.choice(TOOLS),
                                              "input": {"file_path": f"{cwd}/src/{r.choice(WORDS)}.ts",
                                                        "pattern": r.choice(WORDS)}}]}})
        rid = g.uuid()
        rows.append({"parentUuid": aid, "isSidechain": False, "cwd": cwd, "sessionId": sid, "type": "user",
                     "uuid": rid, "timestamp": ts,
                     "message": {"role": "user", "content": [
                         {"type": "tool_result", "tool_use_id": tool_id, "content": g.tool_output()}]}})
        parent = rid
    path = os.path.join(home, ".claude", "projects", claude_dir(cwd), sid + ".jsonl")
    return write_lines(path, rows, mtime_ms)


def codex_session(g, home, cwd, sid, turns, mtime_ms, index):
    r = g.r
    ts0 = mtime_ms - turns * 60_000
    stamp = iso(ts0)
    rows = [{"timestamp": stamp, "type": "session_meta",
             "payload": {"id": sid, "timestamp": stamp, "cwd": cwd, "originator": "codex_cli_rs",
                         "cli_version": "0.80.0", "source": "cli",
                         "instructions": g.text(3)}}]
    rows.append({"timestamp": stamp, "type": "turn_context",
                 "payload": {"cwd": cwd, "approval_policy": "on-request", "model": "gpt-5-codex"}})
    rows.append({"timestamp": stamp, "type": "response_item",
                 "payload": {"type": "message", "role": "user",
                             "content": [{"type": "input_text", "text": "# AGENTS.md instructions for " + cwd + "\n\n" + g.text(2)}]}})
    first = None
    for n in range(turns):
        ts = iso(ts0 + n * 60_000)
        said = g.text(r.randint(1, 2))
        first = first or said
        rows.append({"timestamp": ts, "type": "event_msg", "payload": {"type": "user_message", "message": said}})
        rows.append({"timestamp": ts, "type": "response_item",
                     "payload": {"type": "message", "role": "user", "content": [{"type": "input_text", "text": said}]}})
        call = "call_" + g.uuid().replace("-", "")[:20]
        rows.append({"timestamp": ts, "type": "response_item",
                     "payload": {"type": "function_call", "name": "shell", "call_id": call,
                                 "arguments": json.dumps({"command": ["bash", "-lc", f"rg -n {r.choice(WORDS)} ."], "workdir": cwd})}})
        rows.append({"timestamp": ts, "type": "response_item",
                     "payload": {"type": "function_call_output", "call_id": call,
                                 "output": json.dumps({"output": g.tool_output(), "metadata": {"exit_code": 0}})}})
        rows.append({"timestamp": ts, "type": "response_item",
                     "payload": {"type": "message", "role": "assistant",
                                 "content": [{"type": "output_text", "text": g.text(r.randint(1, 3))}]}})
        rows.append({"timestamp": ts, "type": "event_msg", "payload": {"type": "token_count", "info": {"total": r.randint(1000, 90000)}}})
    day = iso(mtime_ms)
    name = f"rollout-{day[:19].replace(':', '-')}-{sid}.jsonl"
    path = os.path.join(home, ".codex", "sessions", day[0:4], day[5:7], day[8:10], name)
    size = write_lines(path, rows, mtime_ms)
    index.append({"id": sid, "thread_name": g.title(), "updated_at": day})
    return size


def build_opencode(g, home, specs):
    """specs: (cwd, updated_ms, turns, legacy)."""
    d = os.path.join(home, ".local", "share", "opencode")
    os.makedirs(d, exist_ok=True)
    path = os.path.join(d, "opencode.db")
    if os.path.exists(path):
        os.remove(path)
    db = sqlite3.connect(path)
    db.executescript(
        """
        CREATE TABLE session_v2 (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, time_updated INTEGER, directory TEXT);
        CREATE TABLE session_message (session_id TEXT, seq INTEGER, type TEXT, data TEXT);
        CREATE INDEX session_message_session ON session_message (session_id, seq);
        CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, title TEXT, time_updated INTEGER, directory TEXT);
        CREATE TABLE part (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER, data TEXT);
        CREATE INDEX part_session ON part (session_id, time_created);
        """
    )
    r = g.r
    total = 0
    for n, (cwd, updated, turns, legacy) in enumerate(specs):
        sid = f"ses_{g.uuid().replace('-', '')[:26]}"
        # Some are still "New session - <iso>": the rail then uses the first prompt.
        title = f"New session - {iso(updated - 3_600_000)}" if r.random() < 0.2 else g.title()
        table = "session" if legacy else "session_v2"
        db.execute(f"INSERT INTO {table} VALUES (?,?,?,?,?)", (sid, None, title, updated, cwd))
        if r.random() < 0.15 and not legacy:
            # A sub-agent's session counts as its parent's.
            child = f"ses_{g.uuid().replace('-', '')[:26]}"
            db.execute(f"INSERT INTO {table} VALUES (?,?,?,?,?)",
                       (child, sid, "Child session - " + iso(updated), updated + 1000, cwd))
            db.execute("INSERT INTO session_message VALUES (?,?,?,?)",
                       (child, 1, "assistant", json.dumps({"text": g.text(2)})))
        for q in range(1, turns + 1):
            if legacy:
                for kind, txt in (("text", g.text(1)), ("tool", g.tool_output())):
                    data = json.dumps({"type": kind, "text": txt, "sessionID": sid})
                    db.execute("INSERT INTO part VALUES (?,?,?,?)",
                               ("prt_" + g.uuid().replace("-", "")[:20], sid, updated - (turns - q) * 60_000, data))
                    total += len(data)
            else:
                rows = [("user", {"text": g.text(r.randint(1, 2))}),
                        ("assistant", {"text": g.text(r.randint(1, 3)), "tool": {"name": r.choice(TOOLS), "output": g.tool_output()}})]
                for kind, data in rows:
                    s = json.dumps(data)
                    db.execute("INSERT INTO session_message VALUES (?,?,?,?)", (sid, q * 2 - (kind == "user"), kind, s))
                    total += len(s)
    db.commit()
    db.close()
    return total


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("home")
    ap.add_argument("corpus")
    ap.add_argument("--mb", type=int, default=36, help="about this many MB in all")
    a = ap.parse_args()
    home, corpus = os.path.abspath(a.home), os.path.abspath(a.corpus).rstrip("/")
    if os.path.exists(home):
        shutil.rmtree(home)
    os.makedirs(home)

    g = Gen(SEED)
    r = g.r
    budget = a.mb * 1024 * 1024
    split = {"claude": 0.55, "codex": 0.25, "opencode": 0.20}

    def cwd_for(inside):
        if inside:
            sub = r.choice(SUBDIRS)
            return corpus + ("/" + sub if sub else "")
        # Out of scope. The sibling shares the corpus folder's name as a prefix.
        pick = r.random()
        return corpus + "-old" if pick < 0.25 else r.choice(OTHER_FOLDERS)

    def when(n):
        # Newest first as n grows, spread over ~90 days, a little jitter.
        return BASE_MS - n * 90 * DAY_MS // 400 - r.randint(0, 3_600_000)

    # Claude Code
    used, n = 0, 0
    while used < budget * split["claude"]:
        used += claude_session(g, home, cwd_for(r.random() < 0.7), g.uuid(), r.randint(4, 14), when(n))
        n += 1
    claude_n = n
    # Codex
    used, n, index = 0, 0, []
    while used < budget * split["codex"]:
        used += codex_session(g, home, cwd_for(r.random() < 0.7), g.uuid(), r.randint(4, 14), when(n), index)
        n += 1
    with open(os.path.join(home, ".codex", "session_index.jsonl"), "w") as f:
        for row in index[::2]:  # half are named; the rest are titled by first prompt
            f.write(json.dumps(row) + "\n")
    codex_n = n
    # OpenCode
    specs, est, n = [], 0, 0
    while est < budget * split["opencode"]:
        turns = r.randint(4, 14)
        specs.append((cwd_for(r.random() < 0.7), when(n), turns, r.random() < 0.3))
        est += turns * 5500
        n += 1
    build_opencode(g, home, specs)
    oc_n = n

    size = sum(os.path.getsize(os.path.join(p, f)) for p, _, fs in os.walk(home) for f in fs)
    print(f"sessions fixture: {home}: {claude_n} claude, {codex_n} codex, {oc_n} opencode sessions, {size / 1048576:.1f} MB", file=sys.stderr)


if __name__ == "__main__":
    main()
