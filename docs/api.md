# Bridge API, version 1

The bridge serves the ChessBase databases of the machine it runs on to the
oschess web app, over HTTP on the loopback interface. This document is the
contract: the bridge implements it, the oschess web client is written against
it, and the fake bridge in the oschess end-to-end tests implements the same
document with synthetic data.

Endpoints marked *planned* are specified here so that clients can be designed
for them; until the issue named with them is merged, the bridge answers them
with `404 not_found`.

## Transport

- `http://127.0.0.1:<port>`, and the same on `localhost` and `[::1]`. The
  default port is **39581**; `port` in `bridge.toml` overrides it.
- HTTP/1.1. `GET` and `OPTIONS` only. Requests carry no body.
- Every response body is JSON, UTF-8, `Content-Type: application/json;
  charset=utf-8`. Clients must ignore fields they do not know: fields are
  added within version 1 without notice. A change that removes or reinterprets
  a field is version 2, served under `/v2`.
- Limits: the request line and headers together at most 16 KiB; at most 32
  open connections; an idle connection is closed after 5 seconds and a
  request that has not arrived completely after 10 seconds is dropped.

## Access

Every request other than an `OPTIONS` preflight must pass all of these checks.
A request that fails one gets the status and error code listed, and learns
nothing about the databases.

| Check | Failure |
|---|---|
| `Host` is `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>` (guards against DNS rebinding) | `421 misdirected_host` |
| `Origin`, when present, is on the allowlist | `403 forbidden_origin` |
| `Authorization: Bearer <token>` carries the pairing token | `401 unauthorized` |
| The method is `GET` | `405 method_not_allowed` |
| No body (`Content-Length` absent or 0, no `Transfer-Encoding`) | `413 body_not_allowed` |
| Request line and headers within 16 KiB | `431 headers_too_large` |

The allowlist is `https://oschess.org`, `https://www.oschess.org` and
`https://staging.oschess.org`, plus the origins listed under `origins` in
`bridge.toml` (local development, for example `http://localhost:5173`).
A request with no `Origin` header comes from a program that is not a web page;
it still needs the token.

The token is 256 random bits from the operating system, written as 43
characters of unpadded base64url. It is created on first run and kept in the
bridge's data folder.

### CORS and the browser permission

An `OPTIONS` preflight from an allowed origin is answered `204` with:

```
Access-Control-Allow-Origin: <the request's Origin>
Access-Control-Allow-Methods: GET
Access-Control-Allow-Headers: Authorization
Access-Control-Max-Age: 600
Vary: Origin
```

and, when the preflight carries `Access-Control-Request-Private-Network: true`,
also `Access-Control-Allow-Private-Network: true`. A preflight from any other
origin is answered `403 forbidden_origin` without CORS headers. Every other
response to an allowed origin, errors included, carries

```
Access-Control-Allow-Origin: <the request's Origin>
Access-Control-Expose-Headers: Retry-After
Vary: Origin
```

so that the page can read the error body and the retry delay.

Chrome and Edge, from version 142, let a public site reach `127.0.0.1` only
after the user grants the **Local Network Access** permission for that site. No
response header replaces that grant; the web app asks for it during setup and
recognises a denied or revoked permission, which reaches the page as a failed
`fetch` without a response.

## Errors

Every error response has this body:

```json
{ "error": { "code": "database_changing", "message": "The database changed while it was read; retry." } }
```

`code` is one of the values in this document; `message` is English for logs
and never shown to users as is. Some errors add fields next to `code`, named
with them.

| Status | Code | Meaning |
|---|---|---|
| 400 | `bad_request` | A parameter is missing or malformed; `parameter` names it |
| 400 | `query_syntax` | Reserved: the search grammar is lenient and no text is a syntax error today |
| 400 | `unsupported_qualifier` | `q` uses a qualifier ChessBase databases do not have; `qualifier` names it |
| 401 | `unauthorized` | Token missing or wrong |
| 403 | `forbidden_origin` | `Origin` not on the allowlist |
| 404 | `not_found` | No such path, database or game number |
| 405 | `method_not_allowed` | Not `GET` or `OPTIONS` |
| 409 | `database_unavailable` | The database is not `ready`; `state` gives its state. A request for the games of a `cloudOnly` database starts its download and is answered with `downloading` |
| 409 | `superseded` | A newer search (`q`) on the same database replaced this one while it ran; the page shows the newer answer |
| 413 | `body_not_allowed` | The request has a body |
| 421 | `misdirected_host` | `Host` is not a loopback name |
| 422 | `database_too_large` | Searching or sorting this database needs more than the whole search memory budget; number order still works |
| 422 | `not_a_game` | The record is a guiding text or an analysis, which the bridge does not serve as PGN |
| 422 | `unreadable_game` | The game's records are damaged and stay so between reads, or it is too large to serve (a move or annotation record over 2 MiB, or an answer over 8 MiB); `reason` says which, in English |
| 431 | `headers_too_large` | Request line and headers over 16 KiB |
| 500 | `internal` | A bug; the bridge logs it |
| 503 | `database_changing` | ChessBase changed the database during the read; `Retry-After: 1` |
| 503 | `busy` | Too many open connections, too many large answers being sent at once, or search memory taken by other searches; `Retry-After: 1` |

## Database identity and generations

- A database's `id` is 16 lowercase hexadecimal characters derived from its
  normalised path. It is stable while the path stays the same. The path itself
  is never sent.
- A database's `generation` is an opaque string that changes whenever the bridge
  sees the database's files change (sizes or modification times). Responses
  that depend on the contents carry the generation they were read at. A client
  that sees it change in the middle of paging through a list starts the list
  again.

## Consistency

ChessBase writes a database while the bridge reads it, and offers no lock or
snapshot to coordinate with. The bridge therefore promises:

- **A game read during a detected change is retried.** A game is read
  optimistically: its header record and the database's generation are checked
  before and after its move and annotation records are read. If either
  changed, the read is retried up to three times, and then answered
  `503 database_changing`.
- **That detection is best effort.** ChessBase saves a game in several steps
  (the move record, the header, the annotations), and a read that falls
  entirely between two of those steps sees nothing change. It can then serve a
  game that combines the new moves with the old header, for example the old
  result. Every served game is still a valid game: frame checksums and the full
  move check apply to every read. A read after the save has finished is
  correct. The bridge cannot do better without a lock that ChessBase does not
  offer.
- **A list window is as stored at the moment it was read.** A window read while
  ChessBase edits a game may show one row from before that edit and another
  from after it; the next request shows the new state.
- **Caches follow the generation.** Sort orders, suggestions and the position
  index are rebuilt when the generation changes, never mixed with a newer
  record count.

## Search memory

Name tables, sort orders, search results and suggestion counts are kept in
memory within one budget, 1 GiB by default (`OSCHESS_BRIDGE_SEARCH_MIB` sets
another, 16 to 65,536). Every structure reserves its bytes before it is
allocated. When a new one does not fit, what other searches retained is
dropped first and rebuilt when it is next needed; when it still does not fit,
the request is answered `503 busy`. A structure larger than the whole budget
is refused at once with `422 database_too_large`: a sort order needs 12 bytes
per record while it is built and 4 bytes after, so the default budget sorts
databases of up to about 89 million records. A Mega Database of 12 million
records needs about 330 MB with three sort orders and the suggestion counts.

Passes over a database run on workers shared by all requests: the machine's
cores, at most 16, or `OSCHESS_BRIDGE_THREADS`. A search takes the workers that
are free, waits up to 5 seconds for the first one, and is answered `503 busy`
when none comes free; many requests at once therefore wait for each other
instead of multiplying the threads. Each worker reads headers into one 3 MiB
buffer, reserved in the budget before it is allocated. A search takes no more
workers than the whole budget has buffers for, and with little budget left it
runs on fewer, down to one.

## Cancellation

A client names its searches' stream with the `stream` parameter: 1 to 64
characters of `A-Z`, `a-z`, `0-9`, `-` and `_`, chosen by the client, for
example one per browser tab and list. A request that carries `q` and a
`stream` supersedes the search still running in the same stream on the same
database: that one stops at its next batch of headers and is answered
`409 superseded`. An empty `q=` counts: clearing the search box supersedes
the search it replaces. Other streams, requests without a `stream`, requests
without `q`, and suggestions are never superseded, so a page in one tab never
stops a search another tab still waits for. A database remembers its 256
most recently used streams; a forgotten stream starts afresh.

## Endpoints

### `GET /v1/status`

```json
{
  "bridge": { "version": "0.4.0", "api": 1 },
  "databases": { "ready": 9, "opening": 0, "missing": 1, "cloudOnly": 1, "downloading": 1, "unsupported": 2, "unreadable": 0 },
  "download": { "present": 104857600, "total": 734003200 }
}
```

The web app calls it first. `api` below the version it was written for means
the bridge is too old; the app then offers the download link. `databases`
counts the databases in each state. `download` is there while databases are
being downloaded: the bytes on this computer and in all, over all of them.

### `GET /v1/databases`

The databases of ChessBase's own database window, then those `bridge.toml`
adds, then those given with `--database`, each once:

- **The window's** are read from `DBItems.cbini` in `Documents\ChessBase`, in
  the order the file stores them: 2CBH databases first, then the others. The
  window can sort them differently on screen; that setting is not decoded.
- **`bridge.toml`'s** follow in the order written. A folder gives the `.2cbh`
  and `.cbh` database files directly in it, by file name; a folder or a pipe
  named like one is not a database.
- **The list is read again** on a request to `/v1/status` or `/v1/databases`
  after `DBItems.cbini`, `bridge.toml` or a listed folder changed. Each of them
  that cannot be read keeps the databases last read from it, and is read again
  on the next such request until it can be, even if it has not changed since.
  A database that leaves the list
  stays at its end as `missing`, under the same `id`, until the bridge
  restarts, so a page that holds its `id` learns what happened to it.

```json
{
  "databases": [
    {
      "id": "3f9c1a0d5e7b2468",
      "name": "Mega Database 2026",
      "format": "2cbh",
      "state": "ready",
      "records": 11966514,
      "generation": "g1b2c3d4"
    },
    {
      "id": "5e4f30219a8b7c6d",
      "name": "Club 2025",
      "format": "2cbh",
      "state": "downloading",
      "size": 734003200,
      "progress": { "present": 104857600, "total": 734003200 }
    },
    {
      "id": "9a8b7c6d5e4f3021",
      "name": "Openings",
      "format": "pgn",
      "state": "unsupported"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | The name ChessBase's window shows: the title it keeps for the database, else the file name without extension. A database that is not in the window has its file name without extension |
| `format` | `2cbh`, `cbh` or `pgn`; another value is possible later |
| `state` | `ready`; `opening` (being opened, retry after a moment); `missing` (the file is gone, or the database left the list); `cloudOnly` (kept only in the cloud, not on this computer; see below); `downloading` (being brought to this computer; see below); `unsupported` (a format the bridge does not serve: `.cbh` until the bridge renders its games, `.pgn`, which the oschess Library imports itself, and any other); `unreadable` (the files are present but cannot be opened: damaged, locked by another program, or not regular files, such as a folder or a pipe named like one) |
| `records` | Games, guiding texts and analyses; present when `ready` |
| `generation` | See above; present when `ready` |
| `size` | The bytes of the database's files; present when `cloudOnly` or `downloading` |
| `progress` | `present` bytes on this computer of `total`; present when `downloading` |

#### Cloud-only databases

A database in a folder that a cloud storage service keeps in sync may be kept
only in the cloud: its files are placeholders until something reads them, and
reading one downloads it. The bridge recognises placeholders from their file
attributes and never reads one while listing, so listing downloads nothing.

The first request for the games of a `cloudOnly` database (a window or a game)
starts a download and is answered `409 database_unavailable` with state
`downloading`. The bridge reads the database's files one after another in the
background, one database at a time; a database waiting for its turn is
`downloading` too. `/v1/databases` shows the `progress`. When the files are
here the database is `ready`. A download that fails, or cannot start, leaves
it `cloudOnly` (the request is then answered with that state), and the next
request for its games tries again.

The state follows the current marks alone: a database with any file marked
as kept in the cloud is `cloudOnly` (or `downloading` while its download runs
or waits), and one without is opened as usual. So a file the service moves
back to the cloud, or one moved there while the download read another, makes
the database `cloudOnly` again, and the next request for its games downloads
it. A service that keeps a file marked after all of it was read leaves the
database `cloudOnly`; the bridge logs that, and downloads again only when its
games are requested again.

### `GET /v1/databases/{id}/games`

One window of the database's records, sorted and optionally searched.

| Parameter | Default | Meaning |
|---|---|---|
| `offset` | `0` | The first row of the window, counted from 0 in the sorted order |
| `limit` | `200` | Rows in the window, 1 to 500 |
| `sort` | `number` | `<key>`, `<key>-asc` or `<key>-desc`; keys below. It wins over a `sort:` token in `q`; an unknown key is `400 bad_request` |
| `q` | | A search in the Library search grammar ([search-grammar.md](search-grammar.md)); `total` then counts the matches |
| `stream` | | The client's name for this list, which lets a newer search replace an older one ([Cancellation](#cancellation)); an invalid name is `400 bad_request` |

Sort keys: `number`, `white`, `black`, `whiteElo`, `blackElo`, `result`,
`moves`, `eco`, `tournament` (alias `event`), `date`, `round`, `annotator` —
the oschess Library's keys plus `number` and the two Elo keys. Without a
direction, `date` and `moves` sort descending and the others ascending, as in
the Library. Ties are broken by `number`, ascending, in both directions.
Unknown values come first ascending and last descending. A guiding text or an
analysis sorts by its title (as `tournament`) and its author (as `annotator`)
and has no other key.

```json
{
  "generation": "g1b2c3d4",
  "total": 11966514,
  "offset": 0,
  "sort": "number-asc",
  "rows": [
    {
      "number": 1,
      "kind": "game",
      "white": "Morphy, Paul",
      "whiteElo": 0,
      "black": "Anderssen, Adolf",
      "blackElo": 0,
      "result": "1-0",
      "moves": 17,
      "eco": "C52",
      "event": "Paris m",
      "site": "Paris",
      "date": "1858.12.27",
      "round": "7",
      "annotator": "",
      "flags": { "deleted": false, "chess960": false }
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `total` | Rows matching `q` (all records without `q`) |
| `number` | The record's number in the database, from 1, as ChessBase numbers it |
| `kind` | `game`, `text` (a guiding text) or `analysis` |
| `white`, `black`, `event`, `site`, `annotator` | As in the PGN tags; empty when unknown. Names are `Last, First` |
| `whiteElo`, `blackElo` | 0 when unknown |
| `result` | `1-0`, `0-1`, `1/2-1/2` or `*` |
| `moves` | Full moves of the main line, as the header stores it |
| `eco` | `A00`..`E99`, or empty |
| `date` | PGN date, `????.??.??` for unknown parts |
| `round` | `5`, `5(2)` with a sub-round (as in the served PGN), or empty |
| `flags.deleted` | ChessBase marked the record deleted; it is still listed, as ChessBase lists it |
| `flags.chess960` | The game starts from a Chess960 position |

A row for a guiding text or an analysis has `kind: "text"` or `"analysis"`,
its title in `event`, its author in `annotator`, `result: "*"`, and empty game
fields.

Text fields in a row are cut at 200 characters and then end with `…`; the
game's PGN has them in full. A window therefore stays small however long a
name stored in the database is.

### `GET /v1/databases/{id}/games/{number}`

One game as PGN.

| Parameter | Default | Meaning |
|---|---|---|
| `lang` | `en` | Comma-separated ISO 639-1 language preference for annotation text, for example `uk,de`. ChessBase stores comments in English, German, French, Spanish, Italian, Dutch, Portuguese, Polish and Greek; other codes are passed over. A game's texts are written in the first preferred language it has, else in English, else in the first language stored, and texts stored for any language always |

```json
{
  "generation": "g1b2c3d4",
  "number": 1,
  "pgn": "[Event \"Paris m\"]\n...\n1. e4 e5 2. Nf3 {A comment} ... 1-0\n",
  "annotations": "complete"
}
```

| Field | Meaning |
|---|---|
| `pgn` | The game, with its variations and its annotations: comments in `{}`, symbols as NAGs, coloured squares and arrows as `[%csl ...]` and `[%cal ...]` (green, yellow and red; ChessBase's other colours are left out). Annotation types PGN has no form for (training questions, clocks, game quotations, medals and the like) are left out |
| `annotations` | `none` when the game has no annotations or the database no annotation file; `complete` when every annotation was read; `incomplete` when an annotation of unknown layout stopped decoding, and the PGN then has the annotations before it |
| `unreadableAnnotation` | With `incomplete`: the annotation type code, a number |

A guiding text or an analysis is answered `422 not_a_game`, and a game whose
records are damaged `422 unreadable_game`. Deleted games are served.

### `GET /v1/databases/{id}/suggest`

| Parameter | Meaning |
|---|---|
| `field` | `player`, `event` or `annotator` |
| `prefix` | The typed beginning, case-insensitive, at least 1 character; for people, a first name that starts with it counts too |
| `limit` | 1 to 20; 20 by default |

```json
{ "field": "player", "suggestions": [ { "value": "Morphy, Paul", "label": "Morphy, Paul", "games": 211 } ] }
```

`value` is the complete name. Put in double quotes after its qualifier
(`player:"Morphy, Paul"`) it finds the games of that name. `label` is the name
for display, cut at 200 characters with `…`. A name longer than a query value
can hold (256 characters) or containing a double quote cannot be searched
exactly and is not offered. `games` counts the games with the name in that
role (either colour for `player`); guiding texts and analyses are not counted,
and entities with the same name are counted together. Most games first, then
alphabetical.

### `GET /v1/databases/{id}/explorer` (planned, #24)

| Parameter | Meaning |
|---|---|
| `fen` | The position, in FEN |
| `variant` | `standard` (default) or `chess960`; Chess960 is answered `unsupported` in the first index |

```json
{
  "generation": "g1b2c3d4",
  "state": "ready",
  "moves": [ { "uci": "e2e4", "games": 5012345, "white": 1700000, "draws": 2100000, "black": 1212345 } ],
  "topGames": [ { "number": 1234, "white": "Carlsen, Magnus", "whiteElo": 2882, "black": "…", "blackElo": 2800, "result": "1-0", "year": 2014 } ]
}
```

`state` is `ready`, `building` (with `progress` from 0 to 1) or `unsupported`.
Castling is written the standard way, `e1g1` and `e1c1`.

## Pairing

On first run, and from the tray menu's «Open oschess», the bridge opens

```
https://oschess.org/library?source=chessbase#cb-bridge=<token>&port=<port>
```

in the default browser. The fragment never reaches a server. The web app reads
it, stores the token and port for this browser, and removes the fragment from
the address bar before it renders. The tray menu's «Show pairing token» shows
the same token for pasting by hand.

## Fake bridge

The oschess end-to-end tests run a fake bridge that implements this document
with synthetic games and no database. It must answer the access checks,
errors and states above exactly as described here, so that the web app's
handling of each is tested. Changes to this document therefore land in the
bridge first and in the fake bridge next.
