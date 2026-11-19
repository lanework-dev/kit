# Changelog

All notable changes to lwk are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The kit contract (`contract/kit.json`) has its own semantic version, given in each release.

## [Unreleased]

## [0.2.0] - 2026-11-18

Kit contract: **1.2.0** (additive; calls written for 1.0.0 answer as before).

### Added

- `lwk serve`: a long-running server that reads JSON lines on stdin and writes answers on stdout, in the wire format of the Lanework app's kit socket. Calls can run in parallel and answer out of order. `hello` returns the versions, `cancel` stops a call, `shutdown` stops the server, and `--idle-exit SECS` exits after a quiet period. File lists stay in memory between calls. Protocol: [`contract/serve.md`](https://github.com/lanework-dev/kit/blob/v0.2.0/contract/serve.md).
- `lwk find --git` (tool input `git: true`): files with uncommitted changes and files from recent commits rank higher, as in fff-search 0.11.0. git runs as a subprocess. Commits are cached until HEAD moves; status is kept for 2 s.
- `lwk grep` and `lwk sg` can search a single file. The CLI prints the path as you typed it.
- `--max-lines N` for `grep` and `sg` (tool input `max_lines`, default and maximum 2000) caps the total number of lines returned.
- `lwk sg -g/--glob GLOB` (tool input `globs`): the same globs and `!` excludes as `grep`.
- Structural patterns that lack a body, such as `def $F($$$ARGS)`, `fn $N($$$A)` or `function $F($$$A)`, are retried with the body added. The answer's `note` names the pattern that was searched and the declarations it skips.
- Example patterns for each language in the `search_structural` tool description.
- Error code `cancelled` and the optional `note` field in answers.
- `bench/serve_bench.py` and a serve section in BENCHMARKS.md.

### Changed

- The tool input `folder` is now `path`, which may name a folder or a file. `folder` is still accepted as an alias.
- `lwk help` and the tool descriptions say that `--limit` counts files, each returned with all its matching lines.
- A path outside the project now answers with the error code `scope` instead of `not_found`, so lwk does not reveal what exists outside it.

### Fixed

- `lwk grep` and `lwk sg` refused a file path with "That is not a folder".
- `lwk sg` rejected `-g`, although the README listed it as a common option.

## [0.1.0] - 2026-11-18

First release. Kit contract: **1.0.0**.

### Added

- `lwk find QUERY` (`find_files`): fuzzy, typo-tolerant file search, best first, with fff's ranking on [frizbee](https://github.com/saghen/frizbee).
- `lwk grep QUERY` (`search_text`): text search on ripgrep's crates. The query is a literal unless you pass `-E`; globs support `!` excludes.
- `lwk sg PATTERN` (`search_structural`): search by syntax with ast-grep patterns, in ts, tsx, js, jsx, json, css, html, python, rust and go.
- `lwk ls` (`list_files`): the files `rg` would search.
- `lwk sessions QUERY` (`search_sessions`): past Claude Code, Codex and OpenCode conversations about the folder.
- `lwk mcp`: a stdio MCP server named `lanework-kit` that serves the same five tools.
- `--json` output in the shapes of `contract/kit.json`, and `lwk version --json`.
- Answers are capped for an agent's context. They respect `.gitignore`, skip hidden and binary files, and say when they were cut. Exit codes: 0 found, 1 nothing found, 2 error.
- Scope rules: searches stay inside the project folder. A path that climbs out with `..`, a symlink pointing outside, and `$HOME` or `/` are refused.
- An on-disk file-list cache for `find` and `ls`, checked against folder and ignore-file change times; `LWK_NO_CACHE=1` turns it off.
- When the Lanework app is running, lwk asks the app's kit socket first.
- `lwk COMMAND ARGS` runs any other command unchanged.
- `lwk hook on|off|status`: an opt-in Claude Code hook that rewrites plain `grep -r` to `lwk grep` (off by default).
- Releases for macOS (signed with a Developer ID, notarized) and Linux (static musl), a shell installer and a Homebrew tap.

[Unreleased]: https://github.com/lanework-dev/kit/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/lanework-dev/kit/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/lanework-dev/kit/releases/tag/v0.1.0
