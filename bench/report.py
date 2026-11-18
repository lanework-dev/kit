#!/usr/bin/env python3
"""Turn bench/results/ (hyperfine JSON exports and saved outputs, written by
bench/run.sh) into BENCHMARKS.md. Standard library only.

    python3 -I bench/report.py bench/results BENCHMARKS.md
"""

import json
import os
import re
import sys

# Hits under these are noise: nobody asked for them.
SESSIONS_NOTE = [""]
NOISE = re.compile(r"(^|/)(node_modules|dist|\.git|vendor|\.next|coverage|target)(/|$)")
BINARY = re.compile(r"^Binary file (.+) matches$")
GREPLINE = re.compile(r"^(.+?):\d+:")


def read_text(path):
    try:
        with open(path, "rb") as f:
            return f.read()
    except OSError:
        return None


def clean(path):
    return path[2:] if path.startswith("./") else path


def lines_of(raw):
    return [l for l in raw.decode("utf-8", "replace").split("\n") if l.strip()]


def stats(kind, label, outdir):
    """count, bytes, tokens, noise for one command's saved output."""
    raw = read_text(os.path.join(outdir, label + ".out"))
    if raw is None:
        return None
    lines = lines_of(raw)
    size = len(raw)
    res = {"bytes": size, "tokens": size // 4, "count": None, "noise": None}
    if kind == "paths":
        paths = [clean(l) for l in lines]
        res["count"] = len(paths)
        res["noise"] = sum(1 for p in paths if NOISE.search(p))
    elif kind in ("grep", "sg"):
        paths = []
        if label.startswith("ast-grep"):
            js = read_text(os.path.join(outdir, label + ".json"))
            for l in lines_of(js or b""):
                try:
                    paths.append(clean(json.loads(l)["file"]))
                except (ValueError, KeyError):
                    pass
        else:
            for l in lines:
                m = BINARY.match(l) or GREPLINE.match(l)
                if m:
                    paths.append(clean(m.group(1)))
        res["count"] = len(paths)
        res["noise"] = sum(1 for p in paths if NOISE.search(p))
    elif kind == "sessions":
        if label.startswith("lwk"):
            res["count"] = sum(1 for l in lines if re.match(r"^(claude|codex|opencode) ", l))
        else:
            res["count"] = len(lines)
        res["noise"] = None
    return res


def sg_precision(outdir):
    """How well the regex attempt finds what ast-grep finds."""
    js = read_text(os.path.join(outdir, "ast-grep.json"))
    rx = read_text(os.path.join(outdir, "grep-regex.out"))
    if js is None or rx is None:
        return None
    spans = []
    for l in lines_of(js):
        try:
            o = json.loads(l)
            spans.append((clean(o["file"]), o["range"]["start"]["line"] + 1, o["range"]["end"]["line"] + 1))
        except (ValueError, KeyError):
            pass
    hits = []
    for l in lines_of(rx):
        m = re.match(r"^(.+?):(\d+):", l)
        if m:
            hits.append((clean(m.group(1)), int(m.group(2))))
    if not spans:
        return None
    good = sum(1 for f, n in hits if any(f == a and b <= n <= c for a, b, c in spans))
    found = sum(1 for a, b, c in spans if any(f == a and b <= n <= c for f, n in hits))
    return {
        "hits": len(hits),
        "true": good,
        "truth": len(spans),
        "found": found,
    }


def fmt_t(sec):
    ms = sec * 1000
    if ms >= 1000:
        return f"{sec:.2f} s"
    if ms >= 100:
        return f"{ms:.0f} ms"
    if ms >= 10:
        return f"{ms:.1f} ms"
    return f"{ms:.2f} ms"


def fmt_n(n):
    return "—" if n is None else f"{n:,}"


def summarize(r):
    """mean, stddev, min of one result: hyperfine 2 (measurements) or 1 (times)."""
    if "measurements" in r:
        times = [m["time_wall_clock"]["value"] for m in r["measurements"]]
    else:
        times = r.get("times") or []
    if not times:
        return None
    mean = sum(times) / len(times)
    var = sum((t - mean) ** 2 for t in times) / (len(times) - 1) if len(times) > 1 else 0.0
    return {"mean": mean, "stddev": var ** 0.5, "min": min(times), "runs": len(times)}


def load(*paths):
    """The results of one or more hyperfine exports, merged by command."""
    out = {}
    for path in paths:
        raw = read_text(path)
        if raw is None:
            continue
        try:
            for r in json.loads(raw)["results"]:
                s = summarize(r)
                if s:
                    out[r.get("name") or r["command"]] = s
        except (ValueError, KeyError):
            continue
    return out or None


def ratio(x, base):
    if base == 0:
        return "—"
    r = x / base
    return f"{r:.2f}×" if r < 10 else f"{r:.1f}×"


def table(kind, warm, st, with_stats):
    base = warm.get("lwk")
    rows = []
    head = "| Command | Mean ± σ | Min | Runs | vs lwk |"
    sep = "| --- | ---: | ---: | ---: | ---: |"
    if with_stats:
        head += " Results | Bytes | ≈Tokens | Noise |"
        sep += " ---: | ---: | ---: | ---: |"
    rows += [head, sep]
    for name, r in sorted(warm.items(), key=lambda kv: kv[1]["mean"]):
        line = f"| `{name}` | {fmt_t(r['mean'])} ± {fmt_t(r['stddev'] or 0)} | {fmt_t(r['min'])} | {r['runs']} | "
        line += ("—" if name == "lwk" or not base else ratio(r["mean"], base["mean"])) + " |"
        if with_stats:
            s = st.get(name) or {}
            line += f" {fmt_n(s.get('count'))} | {fmt_n(s.get('bytes'))} | {fmt_n(s.get('tokens'))} | {fmt_n(s.get('noise'))} |"
        rows.append(line)
    return "\n".join(rows)


def summary(jid, kind, warm, st, extra):
    """A short summary computed from the numbers, plus the standing caveats."""
    out = []
    lw = warm.get("lwk")
    # `lwk-all` is lwk too (uncapped); it is compared in the table, not here.
    others = {k: v for k, v in warm.items() if not k.startswith("lwk")}
    if lw and others:
        fastest = min(others.items(), key=lambda kv: kv[1]["mean"])
        slower = [k for k, v in others.items() if v["mean"] > lw["mean"] * 1.1]
        faster = [k for k, v in others.items() if v["mean"] < lw["mean"] / 1.1]
        close = [k for k in others if k not in slower and k not in faster]
        parts = []
        if slower:
            parts.append("faster than " + ", ".join(f"`{k}` ({ratio(others[k]['mean'], lw['mean'])})" for k in slower))
        if close:
            parts.append("within 10% of " + ", ".join(f"`{k}`" for k in close))
        if faster:
            parts.append("**slower** than " + ", ".join(f"`{k}` ({ratio(lw['mean'], others[k]['mean'])} slower)" for k in faster))
        out.append(f"lwk ran in {fmt_t(lw['mean'])}: " + "; ".join(parts) + ".")
        if lw["mean"] > fastest[1]["mean"] * 1.1:
            out.append(f"The fastest here is `{fastest[0]}` ({fmt_t(fastest[1]['mean'])}); lwk is not the fastest on this job.")
    s_l = st.get("lwk") or {}
    # Output size against the nearest comparable command.
    big = [(k, v) for k, v in st.items() if not k.startswith("lwk") and v and v.get("tokens") is not None]
    if kind != "startup" and s_l.get("tokens") is not None and big:
        k, v = max(big, key=lambda kv: kv[1]["tokens"])
        if v["tokens"] > 0 and s_l["tokens"] < v["tokens"]:
            out.append(f"Output: lwk ≈{s_l['tokens']:,} tokens against ≈{v['tokens']:,} for `{k}`.")
        elif s_l["tokens"] > 0:
            out.append(f"Output: lwk ≈{s_l['tokens']:,} tokens, as much as or more than the others (largest other: `{k}`, ≈{v['tokens']:,}).")
    noisy = [(k, v["noise"]) for k, v in st.items() if v and v.get("noise")]
    if noisy:
        out.append("Noise (hits under ignored or vendored paths): " + ", ".join(f"`{k}` {n:,}" for k, n in noisy) + ".")
    elif st and any(v and v.get("noise") is not None for v in st.values()):
        if jid.endswith("-live"):
            out.append("No command returned noise here, even in a working checkout with `node_modules` and `dist`.")
        else:
            out.append("No command returned noise on this corpus: a fresh clone has no `node_modules` or `dist`, so `.git` is the only ignored path a tool can walk into. The `-live` jobs run in the working checkout, where it matters.")
    out.extend(extra)
    return " ".join(out)


def cache_note(warm):
    nc, lw = warm.get("lwk-nocache"), warm.get("lwk")
    txt = ("**What the cache does.** `lwk` keeps the walked file list on disk, checked against directory mtimes (and the ignore files) on every call. "
           "The `lwk` row is a *warm repeat call*: the cache exists and nothing changed, so lwk stats the folders instead of walking them again. "
           "The `lwk-nocache` row (`LWK_NO_CACHE=1`) is what a *first call* costs, and about what a call pays after changes to the tree that force a walk (a changed ignore file, or a new folder, which is walked whole; a folder whose contents changed is re-listed on its own). "
           "A first call pays the full walk, and so does any call when the cache is off; the cache saves nothing there. ")
    if nc and lw:
        if nc["mean"] > lw["mean"] * 1.05:
            txt += f"Here the warm cache took {fmt_t(lw['mean'])} against {fmt_t(nc['mean'])} without it ({ratio(nc['mean'], lw['mean'])} faster). "
        elif lw["mean"] > nc["mean"] * 1.05:
            txt += f"Here the warm cache was not faster: {fmt_t(lw['mean'])} against {fmt_t(nc['mean'])} without it. "
        else:
            txt += f"Here the two are level ({fmt_t(lw['mean'])} against {fmt_t(nc['mean'])}). "
    txt += "The benchmark's cache folder is `bench/.corpus/xdg-cache`, emptied at the start of the run, so the user's real cache is never used. `lwk-all` in the `ls` jobs runs with the cache warm too."
    return txt


def notes_for(jid, kind, warm, st, outdir):
    n = []
    if (jid.startswith("find-") or jid.startswith("ls-")) and "lwk-nocache" in warm:
        n.append(cache_note(warm))
    if jid.endswith("-live") and jid.startswith("grep-") and "grep" not in warm:
        n.append("Plain `grep -r` is left out of this job: it reads every file under `node_modules` and took about three minutes for one run on this machine (`BENCH_LIVE_GREP=1` adds it). The others skip ignored folders by default.")
    if jid.startswith("find-"):
        n.append("lwk matches fuzzily (it finds the file despite the typo); `find -iname` and `fd` match substrings, so a typo gives them nothing. `fzf --filter` is fuzzy too but only sees `git ls-files`. No app is answering, so `lwk` is the in-process path: its file list comes from the on-disk cache when warm, and `lwk-nocache` shows the full walk (see the cache note in the summary). With the app's warm index the same query skips the walk too.")
    elif jid.startswith("grep-"):
        n.append("`lwk grep` and `rg` share ripgrep's engine (the `ignore` and `grep-searcher` crates), so on speed they are expected to be level, and any gap is start-up, walk order or lwk's result cap. lwk's win here is the smaller, capped, ignore-aware output, not raw speed. lwk caps its answer (hard maximum 200 files), so its result count is not the total.")
    elif jid.startswith("sg-"):
        p = sg_precision(outdir)
        if p:
            pr = f"{p['true']}/{p['hits']}" if p["hits"] else "0/0"
            n.append(
                f"Regex attempt vs ast-grep: the regex printed {p['hits']} line(s), {pr} of them inside a real match; "
                f"it located {p['found']} of the {p['truth']} matches ast-grep found. A line-based regex cannot see a call that continues on a later line, a nested call, or the difference between code and a comment or string, which is the precision a syntax-aware search buys."
            )
        n.append("`lwk sg` and `ast-grep run` share ast-grep's engine, so expect the same match set and similar speed; lwk prints one line per match where ast-grep prints every line of it, and lwk caps its answer. lwk searches only the contract's ten languages (ts, tsx, js, jsx, json, css, html, python, rust, go); `ast-grep run` tries every language it knows, so its count can be higher.")
    elif jid.startswith("ls-"):
        n.append("`lwk ls` stops at 1,000 paths by default (the agent-sized answer); `lwk ls --limit 50000` is the uncapped list. `rg --files` and `fd -t f` use the same ignore rules; `find -type f` walks everything, `.git` included.")
    elif jid == "sessions":
        n.append(SESSIONS_NOTE[0])
        n.append("`grep -r` has to read every transcript in full, including tool output, so its time grows with all the chats you ever had; `lwk sessions` reads the same files but returns one titled, de-duplicated line per session within the working folder. The counts are not alike: lwk's is sessions, grep's is matching lines.")
    elif jid.startswith("startup-"):
        n.append("lwk's pass-through `exec()`s the command, so the cost is lwk's own start-up (a small static binary) added to the command's. This is what every hooked Bash call pays.")
    return n


def serve_section(res, meta, jobs):
    """One section for the serve job: round trips through one `lwk serve`,
    next to the CLI warm-cache means of the find and ls jobs on the same corpus."""
    names = sorted(n for n in os.listdir(res) if n.startswith("serve-") and n.endswith(".json"))
    if not names:
        return []
    o = ["## lwk serve: repeated calls through one process\n",
         "A `lwk serve` process (the way the Lanework app and editors run it) is kept alive and sent each query once to warm it, then "
         + meta.get("runs.serve", "many requests, one at a time")
         + ". Each time is the round trip: the request line written until its answer line is read, in `bench/serve_bench.py`. "
         "The CLI column is the mean of `lwk` in the find or ls job on the same corpus (file-list cache warm), which pays a process start on every call; "
         "there is no CLI row for the other finds because the find jobs time the misspelled name only (`typo`).\n"]
    cli_of = {}
    for jid, *_ in jobs:
        warm = load(os.path.join(res, jid + ".warm.json"), os.path.join(res, jid + ".warm-slow.json"))
        if warm:
            cli_of[jid] = warm
    summaries = []
    for n in names:
        corpus = n[len("serve-"):-len(".json")]
        data = json.loads(read_text(os.path.join(res, n)) or b"{}")
        rows = data.get("results") or []
        if not rows:
            continue
        o.append(f"### {corpus}\n")
        o.append("| Query | Median | p90 | First call | Results | CLI warm (mean) | CLI ÷ serve median |")
        o.append("| --- | ---: | ---: | ---: | ---: | ---: | ---: |")
        med = {}
        for r in rows:
            med[r["name"]] = r["median"]
            cli_mean = None
            if r["name"] == "find typo":
                cli_mean = (cli_of.get("find-" + corpus) or {}).get("lwk")
            elif r["name"] == "ls":
                cli_mean = (cli_of.get("ls-" + corpus) or {}).get("lwk")
            elif r["name"] == "ls all":
                cli_mean = (cli_of.get("ls-" + corpus) or {}).get("lwk-all")
            q = f" `{r['query']}`" if r.get("query") else ""
            o.append(f"| {r['name']}{q} | {fmt_t(r['median'])} | {fmt_t(r['p90'])} | {fmt_t(r['first'])} | {r['results']:,} | "
                     + (fmt_t(cli_mean["mean"]) if cli_mean else "—") + " | "
                     + (ratio(cli_mean["mean"], r["median"]) if cli_mean else "—") + " |")
        o.append("")
        one, multi = med.get("find one-word"), med.get("find multi-word")
        line = f"**{corpus}.** "
        if one is not None:
            line += f"A one-word find took a median of {fmt_t(one)} through serve"
            if multi is not None:
                line += f" and a multi-word find {fmt_t(multi)}"
            line += ". "
        summaries.append(line)
    o.append("**Summary.** " + "".join(summaries) +
             "Against the CLI with a warm file-list cache, serve answered the same misspelled find and the same listings in a fraction of the time (see the last column): "
             "it saves the process start, the cache read and the index build, which the CLI pays on every call. "
             "It does not make find free: the fuzzy scoring still runs on every call, so a multi-word query costs more than a one-word one, and the time grows with the number of files. "
             "A find with `git: true` costs a little more per call; the first one of a fresh serve also reads `git status` (see First call), and later calls reuse it for a short while. "
             "The times include a JSON line each way over a pipe, no network, and one request in flight at a time. "
             "Medians of one machine on one day: read the ratios, not the milliseconds.\n")
    return o


def main():
    res, dest = sys.argv[1], sys.argv[2]
    wall = sys.argv[3] if len(sys.argv) > 3 else None
    meta = {}
    raw = read_text(os.path.join(res, "meta.txt"))
    for l in (raw or b"").decode().splitlines():
        if "=" in l:
            k, v = l.split("=", 1)
            meta[k] = v
    jobs = []
    for l in (read_text(os.path.join(res, "jobs.tsv")) or b"").decode().splitlines():
        jid, title, corpus, kind, desc = l.split("\t")
        jobs.append((jid, title, corpus, kind, desc))
    if not jobs and not any(n.startswith("serve-") for n in os.listdir(res)):
        sys.exit("no results in " + res)

    o = []
    o.append("# Benchmarks\n")
    o.append("Generated by `bench/run.sh` and `bench/report.py`; do not edit by hand. Numbers are from one machine on one day, so read the ratios, not the milliseconds.\n")
    o.append("## Setup\n")
    o.append("| | |\n| --- | --- |")
    cpu = meta.get("cpu", "unknown")
    o.append(f"| Machine | {cpu}, {meta.get('cores', '?')} cores, {meta.get('ram_gb', '?')} GB RAM |")
    osname = meta.get("os", "")
    o.append(f"| OS | {osname}{' (macOS ' + meta['macos'] + ')' if 'macos' in meta else ''} |")
    o.append(f"| Date | {meta.get('date', '')} |")
    o.append(f"| lwk | {meta.get('lwk', '')} (commit {meta.get('lwk.commit', '?')}), release build, run in-process: no Lanework app answered |")
    o.append(f"| Mode | {meta.get('mode', '?')}{' (BENCH_FULL=1)' if meta.get('mode') == 'full' else ': lanework-public only; BENCH_FULL=1 adds vscode'}; jobs: {meta.get('jobs', 'all')} |")
    o.append(f"| Runs | warm: {meta.get('runs.warm', '')}; cold: {meta.get('runs.cold', '') if meta.get('cold') == 'yes' else 'not run'} |")
    if "cache.dir" in meta:
        o.append(f"| lwk file-list cache | {meta['cache.dir']}; `lwk-nocache` sets `LWK_NO_CACHE=1` |")
    if "sessions.home" in meta:
        o.append(f"| Past chats | {meta['sessions.home']} |")
    if wall:
        o.append(f"| Wall time | {int(wall) // 60} min {int(wall) % 60} s for the whole run (the build included unless `SKIP_BUILD=1`) |")
    o.append("")
    o.append("### Tool versions\n")
    o.append("| Tool | Version |\n| --- | --- |")
    for k, v in meta.items():
        if k.startswith("tool."):
            o.append(f"| {k[5:]} | {v} |")
    o.append("")
    o.append("### Corpora\n")
    o.append("| Corpus | Commit |\n| --- | --- |")
    for k, v in meta.items():
        if k.startswith("corpus."):
            o.append(f"| {k[7:]} | {v} |")
    o.append("")
    o.append("### Method\n")
    o.append(
        "- Timed with [hyperfine](https://github.com/sharkdp/hyperfine). **Warm**: " + meta.get("runs.warmup", "1") + " warm-up run(s), then the runs above (adaptive: at least 3, at most 10, stopping at about 3 s of timing; a command whose first run, the one that saved its output, took 5 s or more is timed by that run alone and shows 1 in the Runs column), files in the OS page cache. "
        + ("**Cold**: the page cache is purged before every run (`" + meta.get("cold.cmd", "") + "`).\n" if meta.get("cold") == "yes" else
           "**Cold**: not run here. It needs a passwordless `sudo purge` (macOS) or `drop_caches` (Linux); see the header of `bench/run.sh`. Run `sudo -v` first to get cold numbers.\n")
        + "- Every command runs from the corpus folder, through a shell, and its exit status is ignored (grep and lwk exit 1 for \"nothing found\"). The start-up jobs run without a shell (`hyperfine -N`).\n"
        "- Each command's output was saved once to count **Results**, **Bytes** and **≈Tokens** (bytes ÷ 4). **Noise** is the number of hits under `node_modules`, `dist`, `.git`, `vendor`, `.next`, `coverage` or `target`.\n"
        "- lwk caps its answers (by default and by a hard maximum), as it is meant to for an agent; the others print everything. Result counts and sizes therefore differ by design, and the time lwk takes includes stopping early where it can.\n"
        "- Text search is case-insensitive for every tool (lwk's default), so the commands carry `-i`.\n"
    )

    if meta.get("sessions.real"):
        SESSIONS_NOTE[0] = "Run over the real chat history on this machine, one run and no warm-up (BENCH_REAL_SESSIONS=1)."
    else:
        SESSIONS_NOTE[0] = ("Run over a synthetic chat home written by `bench/gen_sessions.py` (fixed seed; Claude Code, Codex and OpenCode formats), "
                            "with sessions in the corpus folder, inside it, and outside it, so lwk's search is scoped and `grep -r` reads all of it. "
                            "`grep -r` covers the Claude and Codex files only (OpenCode keeps its sessions in a database).")
    overview = []
    served = False
    for jid, title, corpus, kind, desc in jobs:
        if jid == "sessions":   # the serve section comes before sessions
            o.extend(serve_section(res, meta, jobs))
            served = True
        warm = load(os.path.join(res, jid + ".warm.json"), os.path.join(res, jid + ".warm-slow.json"))
        if not warm:
            continue
        cold = load(os.path.join(res, jid + ".cold.json"))
        outdir = os.path.join(res, "out", jid)
        st = {name: stats(kind, name, outdir) for name in warm}
        st = {k: v for k, v in st.items() if v}
        with_stats = kind in ("paths", "grep", "sg", "sessions")
        group, _, label = jid.partition("-")
        head = f"## {title}" + (f" — {label}" if group in ("find", "grep", "sg", "ls") else "")
        o.append(head + "\n")
        o.append(f"{desc}.\n")
        o.append("**Warm**\n")
        o.append(table(kind, warm, st, with_stats) + "\n")
        if cold:
            o.append("**Cold**\n")
            o.append(table(kind, cold, st, False) + "\n")
        lw = warm.get("lwk")
        others = {k: v for k, v in warm.items() if not k.startswith("lwk")}
        if lw and others:
            fk, fv = min(others.items(), key=lambda kv: kv[1]["mean"])
            nc = warm.get("lwk-nocache")
            overview.append((jid, lw["mean"], nc["mean"] if nc else None, fk, fv["mean"]))
        o.append("**Summary.** " + summary(jid, kind, warm, st, notes_for(jid, kind, warm, st, outdir)) + "\n")

    if not served:
        o.extend(serve_section(res, meta, jobs))
    if overview:
        table_o = ["## Overview\n", "lwk against the fastest other command of each job (warm, mean). In the find and ls jobs, `lwk` has the file-list cache warm and `lwk-nocache` is the first-call cost (full walk).\n",
                   "| Job | lwk | lwk-nocache | Fastest other | lwk vs it | lwk-nocache vs it |", "| --- | ---: | ---: | --- | ---: | ---: |"]
        def verd(lm, fm):
            return f"{ratio(fm, lm)} faster" if lm < fm / 1.05 else (f"{ratio(lm, fm)} slower" if lm > fm * 1.05 else "level")
        for jid, lm, nm, fk, fm in overview:
            table_o.append(f"| {jid} | {fmt_t(lm)} | {fmt_t(nm) if nm else '—'} | `{fk}` {fmt_t(fm)} | {verd(lm, fm)} | {verd(nm, fm) if nm else '—'} |")
        table_o.append("")
        at = next(i for i, l in enumerate(o) if l.startswith("## ") and l != "## Setup\n" and not l.startswith("## Setup"))
        o[at:at] = table_o
    with open(dest, "w") as f:
        f.write("\n".join(o).rstrip() + "\n")
    print("wrote", dest)


if __name__ == "__main__":
    main()
