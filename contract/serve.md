# `lwk serve`: protocol

Version: the contract's (`contract/kit.json`, 1.1.0). `lwk serve` is the kit
socket's wire (`electron/main/kit-socket.ts` in the Lanework app) over
stdin and stdout, so a client that speaks to `kit.sock` speaks to it with
no change in the five tools' params or answers.

## Transport

- One JSON object per line, UTF-8, each way. A line ends with `\n`; a
  request line over 1 MB is answered `invalid` (id `null`) and dropped.
- stdin carries requests, stdout carries answers and **nothing else**.
  Logs and diagnostics go to stderr.
- Several requests may be in flight at once. Answers come back in the
  order the calls finish, not the order they were sent; match them by `id`.
- EOF on stdin: in-flight calls are aborted and `lwk serve` exits 0.
- A blank line is ignored.

## Request and answer

```
→ {"id": 1, "method": "find_files", "params": {...}}
← {"id": 1, "result": ...}
← {"id": 1, "error": {"code": "scope", "message": "..."}}
```

- `id`: a string or a number, echoed back. A line that is not JSON, is not
  an object, or has no usable `id` is answered with `id: null` and
  `invalid`. An `id` of a call still in flight is refused `invalid`.
- `method`: one of the methods below. Anything else is `invalid`
  (`Unknown method: NAME`).
- `params`: an object (omitted or `null` is `{}`).
- Every answer has exactly one of `result` and `error`.

### Errors

`error.code` is one of the contract's `errorCodes`:

| code          | meaning                                                      |
| ------------- | ------------------------------------------------------------ |
| `invalid`     | the params or the line are wrong                             |
| `scope`       | the folder is outside the allowed roots                      |
| `not_found`   | the folder does not exist                                    |
| `unavailable` | a tool this one needs is missing here                        |
| `timeout`     | the search ran out of time before any match                  |
| `aborted`     | the caller went away                                         |
| `cancelled`   | the call was cancelled by a `cancel` request                 |
| `busy`        | too many calls at once                                       |
| `internal`    | anything else                                                |

`cancelled` is new in contract 1.1.0 and only `lwk serve` sends it.

## Tool methods

`find_files`, `search_text`, `search_structural`, `list_files` and
`search_sessions`. Params and results are the tools' (`inputSchema` in
`contract/kit.json`), plus what the app supplies on `kit.sock`:

- `cwd` (required, absolute): the folder a tool runs in when it names none.
  Missing or relative is `invalid`.
- `roots` (optional, 1 to 32 absolute folders): the folders the call may
  reach, as `kit.sock` takes them. When omitted the scope is `cwd` alone.
  The caller, not `lwk serve`, decides what a client may reach: it supplies
  the narrowest roots it means. A folder outside `roots` is `scope`.

`cwd` and `roots` are not part of the tool's own params. `find_files`
takes `git` (boolean, default false).

## Control methods

### `hello`

Params: none. Answer: `{"lwk": "<version>", "contract": "<contract version>", "pid": <number>}`.
A client sends it first to check the contract it pins.

### `cancel`

Params: `{"id": <id of a request in flight>}`. The target then answers
`error` with code `cancelled`, and its work is stopped (its children are
killed). `cancel` itself answers `{"result": {"cancelled": true}}`; if the
id is not in flight (finished, or never sent) the answer is
`{"result": {"cancelled": false}}`, not an error. A missing `id` is
`invalid`. A target that finished before the cancel was read keeps its
own answer.

### `shutdown`

Params: none. Answers `{"result": null}`, then in-flight calls are aborted
(they answer nothing further) and `lwk serve` exits 0.

## Options

- `--idle-exit SECS`: exit 0 once SECS seconds pass with nothing in flight
  and no line received. Any received line, even an invalid one, restarts
  the clock; so does the end of the last in-flight call. `0` (the
  default) means never.

## RAM cache

`lwk serve` is long-lived, so it keeps what a one-shot `lwk` has to rebuild
in memory: the folders' file lists (as `find_files` and `list_files` read
them) and the fuzzy index over them, per real folder. An entry is checked
against the folders' and ignore files' change times on every call, as the
disk cache is, and rebuilt when stale; it is dropped after a period unused
and the whole cache is bounded. Nothing is kept across runs by this cache
and it changes no answer, only the time to give it. `LWK_NO_CACHE=1` turns
it off. It never reads or writes the app's own index.

## Examples

A search that finds files, with the cwd and a narrower root:

```
→ {"id": 1, "method": "hello"}
← {"id": 1, "result": {"lwk": "0.1.0", "contract": "1.1.0", "pid": 4242}}
→ {"id": 2, "method": "find_files", "params": {"query": "kit sock", "limit": 2, "git": true, "cwd": "/work/app", "roots": ["/work/app"]}}
← {"id": 2, "result": [{"path": "src/socket.rs", "score": 412, "ranges": [[4, 8]]}, {"path": "contract/serve.md", "score": 190}]}
```

Two requests in flight, answers out of order, one error:

```
→ {"id": "a", "method": "search_text", "params": {"query": "TODO", "cwd": "/work/app"}}
→ {"id": "b", "method": "list_files", "params": {"folder": "/etc", "cwd": "/work/app"}}
← {"id": "b", "error": {"code": "scope", "message": "/etc is outside the allowed folders."}}
← {"id": "a", "result": [{"path": "src/main.rs", "lines": [{"line": 12, "text": "// TODO", "ranges": [[3, 7]]}], "truncated": false}]}
```

Cancel a slow call, then shut down:

```
→ {"id": 7, "method": "search_text", "params": {"query": "a.*b", "regex": true, "cwd": "/work/big"}}
→ {"id": 8, "method": "cancel", "params": {"id": 7}}
← {"id": 7, "error": {"code": "cancelled", "message": "Cancelled."}}
← {"id": 8, "result": {"cancelled": true}}
→ {"id": 9, "method": "shutdown"}
← {"id": 9, "result": null}
```
(then the process exits 0)
