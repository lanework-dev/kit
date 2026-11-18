# lanework-kit (`lwk`)

Fast file, text, structural and session search for coding agents, as one small
native binary. It is the search kit of the [Lanework](https://github.com/fuongz/lanework)
desktop app, on the command line and as an MCP server.

`lwk` asks the running Lanework app first (its warm index). With no app to
answer, it runs the same tools in-process, in your working folder.

## Install

Homebrew (macOS, Linux):

```sh
brew install lanework-dev/tap/lanework-kit
```

Shell script (macOS, Linux):

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/lanework-dev/kit/releases/latest/download/lanework-kit-installer.sh | sh
```

Builds: Apple silicon and Intel macOS, and x86_64 and aarch64 Linux (musl,
static). macOS binaries are signed and notarized. Each archive holds `lwk`,
`LICENSE` and `THIRD_PARTY_LICENSES.md`.

From source: `cargo install --path .`

## Commands

```text
lwk find QUERY [FOLDER]        Fuzzy-find files by path, best first
lwk grep QUERY [FOLDER]        Search file contents (a literal unless -E)
lwk sg PATTERN [FOLDER]        Search code by syntax (ast-grep pattern)
lwk ls [FOLDER]                List the files rg would search
lwk sessions QUERY [FOLDER]    Search Claude Code, Codex and OpenCode sessions
lwk hook on|off|status         Claude Code hook that runs plain grep -r as lwk
lwk mcp                        Serve the kit's tools as an MCP server (stdio)
lwk COMMAND [ARGS...]          Run any other command unchanged
```

Options: `--json`, `--limit N`, `-E/--regex`, `-F/--fixed-strings`,
`-i/--ignore-case`, `-s/--case-sensitive`, `-g/--glob GLOB`, `--lang LANG`
(`lwk help` has the full text). `.gitignore` is respected and hidden files are
skipped. Exit code: 0 found, 1 nothing found, 2 error.

### File-list cache

`lwk find` and `lwk ls` keep each folder's file list on disk, so a repeat call
does not walk the tree again. Like git's untracked cache, every call checks the
change time of each folder and each ignore file (`.gitignore`, `.ignore`,
`.rgignore`, `.git/info/exclude`, the global excludes file). Only folders that
changed are read again, and a changed ignore file means a full walk. The
answer is always the same as without the cache.

- Location: `$XDG_CACHE_HOME/lwk`, else `~/Library/Caches/lwk` (macOS) or
  `~/.cache/lwk` (Linux). One file per folder; deleting them is always safe.
- `LWK_NO_CACHE=1` turns it off.
- The first call for a folder pays the full walk, and so does the first call
  after an ignore file changes. Trees of more than 500,000 files are not
  cached.
- When the Lanework app answers over its socket, it uses its own index.

## MCP setup

`lwk mcp` serves the kit over stdio. The server name is `lanework-kit`; its
tools are `find_files`, `search_text`, `search_structural`, `list_files` and
`search_sessions`. Each runs in the folder the agent was started in.

**Claude Code**

```sh
claude mcp add lanework-kit -- lwk mcp
```

**Codex** (`~/.codex/config.toml`)

```toml
[mcp_servers.lanework-kit]
command = "lwk"
args = ["mcp"]
```

**OpenCode** (`opencode.json`)

```json
{
  "mcp": {
    "lanework-kit": { "type": "local", "command": ["lwk", "mcp"], "enabled": true }
  }
}
```

## Scope rules

The tools search the project folder only:

- A `FOLDER` must be inside the working folder. `..` that climbs out of it is
  refused.
- A symlink that points outside the project folder is refused.
- Your home folder (`$HOME`) itself and `/` are never searched.

(When the Lanework app is running and the folder belongs to the same project,
the app may answer for it.)

## The hook is off by default

`lwk hook on` adds one entry to Claude Code's `PreToolUse` hooks (matcher
`Bash`, command `lwk rewrite`) in `$CLAUDE_CONFIG_DIR/settings.json`
(default `~/.claude/settings.json`). It then turns a plain recursive
`grep -rn "literal" folder` into `lwk grep`. `lwk hook off` removes that entry
and nothing else; other hooks and every other key stay as they were, and a
settings file that does not parse is refused, never repaired. `lwk hook status`
prints `on` or `off`.

Why it is off until you ask:

- It runs before **every** Bash call, and changes what the agent runs.
- `lwk grep` is not `grep`: `.gitignore`d, hidden and binary files are
  skipped, lines print as `path:line: text`, and the answer stops at 200 files.
- Rewriting commands is a decision for the user, not a default. The lesson
  comes from [rtk](https://github.com/rtk-ai/rtk), which rewrites commands
  to save tokens: that has to be opt-in only.

Only a command whose every token is understood is rewritten; anything else
passes through untouched, and a failing hook never blocks a call. The rewritten
command still goes through your own permission rules.

## Benchmarks

See [BENCHMARKS.md](BENCHMARKS.md).

## Credits

lwk stands on these projects. Licenses are those of the versions in
`Cargo.lock`.

| Project | Used for | License |
| --- | --- | --- |
| [ripgrep crates](https://github.com/BurntSushi/ripgrep) by BurntSushi (`ignore`, `globset`, `grep-searcher`, `grep-regex`, `grep-matcher`) | file walking and text search | Unlicense OR MIT |
| [ast-grep](https://github.com/ast-grep/ast-grep) (`ast-grep-core`, `ast-grep-language`) | structural search | MIT |
| [tree-sitter](https://github.com/tree-sitter/tree-sitter) and the grammars for ts/tsx, js/jsx, json, css, html, python, rust and go (`tree-sitter-typescript`, `-javascript`, `-json`, `-css`, `-html`, `-python`, `-rust`, `-go`, from the [tree-sitter org](https://github.com/tree-sitter)) | parsing | MIT (each repository's own license) |
| [neo_frizbee](https://github.com/saghen/frizbee) (`neo_frizbee`), the SIMD Smith-Waterman matcher [fff](https://github.com/dmtrKovalenko/fff) uses | fuzzy file finding | MIT |
| [fff](https://github.com/dmtrKovalenko/fff)'s ranking, ported (`fff-search`'s `FilePicker::fuzzy_search` and query parser); no fff code is a dependency | how `lwk find` orders and parses queries | MIT |
| [rtk-ai/rtk](https://github.com/rtk-ai/rtk) | inspiration only, no code used; its lesson: rewriting is opt-in only | Apache-2.0 |
| [Lanework desktop app](https://github.com/fuongz/lanework) | origin: lwk began as the app's kit client | MIT (this repo's `LICENSE` and the app's are both "Copyright (c) 2026 fuongz") |

Every dependency's license text is in
[THIRD_PARTY_LICENSES.md](THIRD_PARTY_LICENSES.md).

## License

MIT, see [LICENSE](LICENSE).
