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
| 409 | `database_unavailable` | The database is not `ready`; `state` gives its state. A request for the games of a `cloudOnly` database starts its download and is answered with `downloading`. For the explorer, `state: "indexing"` with `progress` while its position index is built |
| 409 | `superseded` | A newer search (`q`) on the same database replaced this one while it ran; the page shows the newer answer |
| 413 | `body_not_allowed` | The request has a body |
| 421 | `misdirected_host` | `Host` is not a loopback name |
| 422 | `database_too_large` | Searching or sorting this database needs more than the whole search memory budget; number order still works |
| 422 | `unsupported` | The explorer's position or variant is Chess960, which the position index does not hold; `variant` names it |
| 422 | `not_a_game` | The record is a guiding text or an analysis, which the bridge does not serve as PGN |
| 422 | `unreadable_game` | The game's records are damaged and stay so between reads, or it is too large to serve (a move or annotation record over 2 MiB, or an answer over 8 MiB); `reason` says which, in English |
| 431 | `headers_too_large` | Request line and headers over 16 KiB |
| 500 | `internal` | A bug; the bridge logs it |
| 503 | `database_changing` | ChessBase changed the database during the read; `Retry-After: 1` |
| 503 | `index_unavailable` | The position index could not be built; the message says why, and the next request after a minute tries again |
| 503 | `busy` | Too many open connections, too many large answers being sent at once, or search memory taken by other searches; `Retry-After: 1` |

## Database identity and generations

- A database's `id` is 16 lowercase hexadecimal characters derived from its
  normalised path. It is stable while the path stays the same. The path itself
  is never sent.
- A database's `generation` is an opaque string that changes whenever the bridge
  sees the database's files change (sizes or modification times). Responses
  that depend on the contents carry the generation they were read at. A client
  that sees it change in the middle of paging through a list starts the list
  again. The files are those the bridge reads: `.2cbh`, `.2cbg`, `.2cba`,
  `.2lid`, `.2lgd` and `.2lcd` of a 2CBH database; `.cbh`, `.cbg`, `.cba`, `.cbp`,
  `.cbt`, `.cbc`, `.cbs` and, when present, `.cbj` of a classic one (see
  [Classic databases](#classic-databases)); a PGN file itself (see
  [PGN files](#pgn-files)).

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
buffer, reserved in the budget before it is allocated; a worker loading names
reserves 64 KiB for one record, which is read to at most 4 KiB. What a worker
builds, such as its matches or its share of a name table, grows in steps of a
256th of the budget, from 64 KiB to 1 MiB. A search's workers take at most half the
budget with their buffers and one step each, which leaves the other half for
the rest of what they build, and with little budget left a search runs on
fewer workers, down to one.

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
  "download": { "present": 104857600, "total": 734003200 },
  "indexing": [ { "id": "0a1b2c3d4e5f6071", "phase": "reading", "done": 4000000, "total": 11966514 } ],
  "engine": { "name": "Stockfish 19", "threads": { "default": 6, "max": 8 }, "hash": { "default": 512, "max": 8192 } }
}
```

The web app calls it first. `api` below the version it was written for means
the bridge is too old; the app then offers the download link. `databases`
counts the databases in each state; `opening` counts the PGN files being read
for their header index (see [PGN files](#pgn-files)). `download` is there while databases are
being downloaded: the bytes on this computer and in all, over all of them.
`indexing` is there while position indexes are checked or built (see
`GET /v1/databases/{id}/explorer`). `engine` names the engine the analysis
board can use (see `GET /v1/engine/analyze`), or is `null` when `bridge.toml`
names none. The name is the one the engine gave for itself, or its file's
name until it has run once. `threads` and `hash` (MB) are what an analysis
may ask for on this computer (#58): `max` is the logical processors, and the
largest power of two at or below half the physical memory, from 16 to 32768
MB. `default` is what an analysis naming none gets, from `bridge.toml` or
computed, and never above `max`.

### `GET /v1/databases`

The databases of ChessBase's own database window, then those `bridge.toml`
adds, then those given with `--database`, each once:

- **The window's** are read from `DBItems.cbini` in `Documents\ChessBase`, in
  the order the window shows them where its sort setting is decoded:
  ChessBase's default (`Sort` 6 with every `SortDir` 0) orders them by icon,
  largest first, then by title, last first (`docs/format-notes.md`). Under any
  other setting they come in the order the file stores them: 2CBH databases
  first, then the others.
- **`bridge.toml`'s** follow in the order written. A folder gives the `.2cbh`
  and `.cbh` database files and the `.pgn` files directly in it, by file name;
  a folder or a pipe named like one is not a database.
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
      "state": "opening",
      "progress": { "present": 52428800, "total": 157286400 }
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | The name ChessBase's window shows: the title it keeps for the database, else the file name without extension. A database that is not in the window has its file name without extension |
| `format` | `2cbh`, `cbh` or `pgn`; another value is possible later |
| `state` | `ready`; `opening` (a PGN file being read for its header index, see [PGN files](#pgn-files); retry after a moment); `missing` (the file is gone, or the database left the list); `cloudOnly` (kept only in the cloud, not on this computer; see below); `downloading` (being brought to this computer; see below); `unsupported` (a format the bridge does not serve: any but `.2cbh`, `.cbh` and `.pgn`); `unreadable` (the files are present but cannot be opened: damaged, locked by another program, or not regular files, such as a folder or a pipe named like one; for a PGN file, its header index could not be built) |
| `records` | Games, guiding texts and analyses; present when `ready` |
| `generation` | See above; present when `ready` |
| `size` | The bytes of the database's files; present when `cloudOnly` or `downloading` |
| `progress` | `present` bytes on this computer of `total`, when `downloading`; `present` bytes of the PGN file read of `total`, when `opening` |

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

#### Classic databases

A classic database (`format: "cbh"`, ChessBase's format before the 2CBH one)
is served like a 2CBH one: its list, searches, sorts, suggestions, games and
position index answer as a 2CBH copy of the same content answers, apart from
what the format stores otherwise:

- **Its files.** The bridge reads `.cbh`, `.cbg`, `.cba`, the entity files
  `.cbp`, `.cbt`, `.cbc` and `.cbs`, and `.cbj` when the move or annotation
  file is over 4 GiB. These make up its generation, count for its `size`, and
  decide whether it is `cloudOnly`; the search boosters and other files
  ChessBase adds beside them are neither read nor downloaded. A `.cbh` file
  without the files it needs is `unreadable`.
- **Names are cut** at the classic fields' widths: a player's last name at 30
  bytes and first name at 20, a tournament's title at 40 and place at 30, an
  annotator at 45. A name longer in the 2CBH copy is cut in the classic one,
  and a character that no single-byte code page holds may be stored in
  another form.
- **An annotator is one text**, kept in a table of its own rather than as a
  player, and often written `First Last`. Rows, `annotator:` searches, the
  `annotator` sort and annotator suggestions use it as stored; an annotator
  written `Last, First` offers its first name to suggestions, as a player
  does.
- **A guiding text** keeps its titles, one per language, in its own record:
  its row shows the first that is not blank, and its author is its annotator.
  The format has no analyses.
- **`moves`** is at most 255, as the header stores it; the game's PGN has
  every move.

#### PGN files

A PGN file (`format: "pgn"`) is served like a 2CBH database: its list,
searches, sorts, suggestions, games and position index answer as a 2CBH copy
of the same games answers, apart from what the format has otherwise:

- **Opening.** The bridge reads the whole file once to find its games and
  their tags, and keeps what it found, the header index, in the data folder's
  `pgn` folder as `<id>.head`. While it reads, the database is `opening`, with
  the bytes read as `progress`, and requests for its games, searches,
  suggestions and positions are answered `409 database_unavailable` with
  `state: "opening"`. Files are read one at a time, in the background. A
  header index built for the file's current generation is used at once, also
  after the bridge restarts; a change to the file reads it again. A file whose
  header index cannot be built is `unreadable` for a minute, and the next
  request tries again. A header index takes 48 bytes a game plus the names.
  The `pgn` folder is swept as the `index` folder is (see "Storage" under
  `GET /v1/databases/{id}/explorer`): a build's `<id>.head.partial` goes at
  once unless that file is being read, and the header index of a database
  off the list for ten minutes goes.
- **Games only.** Every record is a game: none is deleted, and a PGN file has
  no guiding texts or analyses. A game starts at its first tag, or at its
  first move when it has none, and ends at its result or where the next
  game's tags start after its moves; a tag the game already has also starts
  the next game, and a comment between tags does not. A line ends at LF, CR
  or both, and a tag pair may span lines, with comments and `%` escape lines
  between any of its tokens. A `{` comment ends at its `}`,
  whatever it holds; one still open at the end of the file was left open by
  mistake, and ends before its first line starting `[Event "`, from where the
  file is read on (at most 64 times in a file).
- **Fields.** A row's fields come from the tags. `White` and `Black` are
  players, split at the first comma into `Last, First`; `?`, `-` or nothing is
  no name. `Event` and `Site` are the tournament's title and place. `Date`,
  `Round` (`5.2` or `5(2)` is round 5, sub-round 2), `WhiteElo`, `BlackElo`
  and `ECO` are read as PGN writes them, and a value out of range is unknown.
  `Result` is the tag's, else the result ending the movetext, else `*`;
  `0-0` is ChessBase's result for a game both players lost, and in a game
  whose `Result` tag is `0-0` the movetext's last `0-0` is that result and
  any earlier one castles. `Annotator` is one text in a table of its own, as
  in a classic database. `moves` counts
  the main line's moves as written, and `flags.chess960` is set by a `Variant`
  tag naming Chess960. A tag value is read to 4 KiB.
- **Text.** `GET /v1/databases/{id}/games/{number}` serves the game's text as
  the file writes it, from its first tag to its result, with each line ending
  in `\n`: decoded as UTF-8, or, when the game is not valid UTF-8, in the
  computer's ANSI code page (Windows-1252 where the page has no table). Its
  row's names are read in the same encoding.
  `lang` and `annotations` change nothing, and `annotations` is `complete`:
  a PGN game's comments are all it has.
- **Position index.** Each game's main line is played as written, from its
  `FEN` tag or the standard start, and ends at the first move that names no
  legal move, a null move among them. Games of Chess960 and of other variants
  are left out.
- **`line`.** A row's `line` is the main line read the same way, written in
  the bridge's SAN, so its form may differ from the text's: `O-O` for `0-0`,
  `e8=Q` for `e8Q`. A game with a `FEN` tag other than the standard position
  has none.

### `GET /v1/databases/{id}/games`

One window of the database's records, sorted and optionally searched.

| Parameter | Default | Meaning |
|---|---|---|
| `offset` | `0` | The first row of the window, counted from 0 in the sorted order |
| `limit` | `200` | Rows in the window, 1 to 500 |
| `sort` | `number` | `<key>`, `<key>-asc` or `<key>-desc`; keys below. It wins over a `sort:` token in `q`; an unknown key is `400 bad_request` |
| `q` | | A search in the Library search grammar ([search-grammar.md](search-grammar.md)); `total` then counts the matches |
| `stream` | | The client's name for this list, which lets a newer search replace an older one ([Cancellation](#cancellation)); an invalid name is `400 bad_request` |
| `line` | | Plies of each game's main line to add to its row, 1 to 60 (below); any other value is `400 bad_request`. Without it, rows are as shown |

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
| `white`, `black`, `event`, `site`, `annotator` | As in the PGN tags; empty when unknown. Names are `Last, First`; a classic database's annotator is as stored |
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

With `line`, every game row ends with one more field (#81), so that one window
brings its games' openings, for example to build a player's opening tree:

| Field | Meaning |
|---|---|
| `line` | The first `line` plies of the main line in SAN, one space between moves, as `GET /v1/databases/{id}/games/{number}` writes them: `"e4 e5 Nf3 Nc6 Bb5"`. It is shorter when the game is, and ends at a null move and before the first damage: a move that cannot be read or played, or a broken move tree. It is `""` for a game without moves, and `null` when the game does not start from the standard position (a set-up position or Chess960) or its moves cannot be read at all, damage before the first move included; such a game never fails the window. Only the plies asked for are read: the rest of the game is neither replayed nor checked |

Rows of guiding texts and analyses have no `line`. The lines are read after
the search and the sort, only for the window's games. A database whose
generation changed while they were read is answered `503 database_changing`,
as a game read during a change is ([Consistency](#consistency)). A bridge
older than this field ignores the parameter: a game row without `line` tells
the client so.

Text fields in a row are cut at 200 characters and then end with `…`; the
game's PGN has them in full. A window therefore stays small however long a
name stored in the database is. Rows, like searches, read a name's entity
record to at most 4 KiB; a longer record, which only a damaged file holds, is
an empty name.

### `GET /v1/databases/{id}/games/{number}`

One game as PGN.

| Parameter | Default | Meaning |
|---|---|---|
| `lang` | `en` | Comma-separated ISO 639-1 language preference for annotation text, for example `uk,de`. ChessBase stores comments in English, German, French, Spanish, Italian, Dutch, Portuguese, Polish and Greek; other codes are passed over. In the reading form, a game's texts are written in the first preferred language it has, else in English, else in the first language stored, and texts stored for any language always |
| `annotations` | `reading` | `reading` writes the PGN for reading, in one language. `full` writes every annotation the bridge reads, below; any other value is `400 bad_request` |

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
| `pgn` | The game, with its variations and its annotations: comments in `{}`, symbols as NAGs, coloured squares and arrows as `[%csl ...]` and `[%cal ...]` (green, yellow and red; ChessBase's other colours are left out). Game quotations and medals are written as ChessBase's own export writes them (below). In the reading form, the other types PGN has no form for (training questions, clocks, evaluations and the like) are left out; the full form keeps them. An annotation stored past the game's last move follows the main line's last move, its texts after that move |
| `annotations` | `none` when the game has no annotations or the database no annotation file; `complete` when every annotation was read; `incomplete` when an annotation of unknown layout stopped decoding, and the PGN then has the annotations before it |
| `unreadableAnnotation` | With `incomplete`: the annotation type code, a number |

A guiding text or an analysis is answered `422 not_a_game`, and a game whose
records are damaged `422 unreadable_game`. Deleted games are served. Both forms
are held to the same answer limit.

#### Both forms

- **Game quotations** (type `13`) are a comment on their move, as ChessBase
  writes them: the result (`1-0`, `0-1` or `1/2`), both players as
  `Last,F (Elo)`, the event with its site unless the event names it, `blitz` or
  `rapid` for such an event unless it names it, the year unless the event names
  it, and the round as `(round)` or `(round.subround)`, or a correspondence
  board as `[board]`.
- **Medals** (type `22`) are `[%mdl <bits>]`, as ChessBase writes them: the
  `int` of medal bits. The reading form writes them first in the comment of
  their move, and the full form among the move's commands.
- **The main line's evaluations** (type `26`) are `[%evp 0,<last>,<values>]`
  in a comment of its own before the first move, as ChessBase writes them. It
  holds one value per position of the main line, the start position first:
  - centipawns from White's point of view;
  - a mate in `n` plies as `30000 − n`, negated when Black mates;
  - 32767 for no evaluation.

  ChessBase's own export of a weekly update matches these values in all its
  games with evaluations. That export holds no negative mate, mate on the board
  or entry of an unknown kind. A negative mate is written by symmetry, and mate
  on the board and the unknown kinds as 32767.

#### The full form

Nothing the bridge reads is left out of the full form
(asavis/oschess-cb-bridge#42). What it adds to the reading form:

- **Texts:** every text in every language, each as its own comment led by
  `[%lang xx]`, in stored order; a text before a move stays before it. `xx` is
  the ISO 639-1 code where ChessBase names the language, `any` for a text meant
  for every language, `cb-<nation>` for a classic text whose nation names no
  such language, and `cb-l<number>` for another 2CBH language number. The
  visible comment is cleaned for PGN as in the reading form: braces become
  parentheses, and line breaks, control characters and runs of spaces become
  one space. When that changes the text, when the text is empty or all
  whitespace, when it holds `[%`, or when it is meant to precede its move, a
  comment `[%cbtext lang=xx;value=…]` follows the visible one with the original
  value, percent-encoded, and `before=1` for a text before its move. When
  cleaning leaves nothing to show, the `[%cbtext]` stands in the text's place
  with `alone=1`. `value` is always written, even when empty. A reader takes a
  `[%cbtext]` without `alone` as the value of the visible comment right before
  it.
- **Symbols and graphics** are shown as in the reading form, NAGs and
  `[%csl]`/`[%cal]`, and each annotation is also a command that keeps what they
  cannot: `[%cbsymbols]` with the three NAG slots (move, position, prefix),
  including symbols on the game as a whole, where no NAG can stand, and
  `[%cbsquares]`/`[%cbarrows]` with every mark of every colour. ChessBase's
  colours 7, 8 and 9 have no `[%csl]`/`[%cal]` letter.
- **Engine scores and times** of each move, as `[%eval …]` and `[%emt …]` in a
  comment right after the move and its NAGs:
  - `[%eval]` is in pawns with two decimals, or `#n` for a mate in `n` moves,
    from White's point of view, as oschess and Lichess read it;
  - the move's own engine evaluation (type `21`) comes first, else the main
    line's evaluation of the position after the move (type `26`);
  - a mate on the board and the unknown kinds give no `[%eval]`;
  - `[%emt h:mm:ss]` is the time spent on the move (type `07`).

  Both come from 2CBH only; the classic layouts of types `07` and `21` are not
  confirmed. The annotations' data stays in their commands.
- **Commands** for every annotation that is not a text, in one comment after
  the move's texts, in stored order. Each is `[%cb<name> key=value;…]`, except
  `[%mdl]`. Keys are ASCII letters, values are UTF-8 percent-encoded with
  everything outside `A-Z a-z 0-9 - . _ ~` escaped, empty values are left out
  (except a `[%cbtext]` value), and a reader keeps and ignores a key it does
  not know. Every command but `[%cbtext]` and `[%mdl]` carries `data` in
  base64url without padding: the annotation's bytes after its type, or for
  symbols and graphics the layout below. A later decoder loses nothing.

| Command | Type | Keys besides `data` |
|---|---|---|
| `[%cbsymbols …]` | `03` symbols | none: `data` is the three NAG slots, move, position and prefix, 0 for none |
| `[%cbsquares …]` | `04` coloured squares | none: `data` is the (colour, square) pairs, squares numbered from 1 file by file (`a1` 1, `a2` 2, `b1` 9) |
| `[%cbarrows …]` | `05` arrows | none: `data` is the (colour, from, to) triples, numbered the same way |
| `[%cbquote …]` | `13` game quotation | `result` (`1-0`, `0-1`, `1/2-1/2`, `*`), `white` and `black` as `Last, First`, `whiteElo`, `blackElo`, `event`, `site`, `date` (`YYYY.MM.DD`, `??` unknown), `round`, `subround`, `eco`, and `moves`, the quoted moves as SAN movetext, for a 2CBH quotation from the standard position whose moves replay |
| `[%mdl <bits>]` | `22` medals | written as ChessBase writes it, the whole annotation |
| `[%cbcritical …]` | `18` critical position | `phase` (`opening`, `middlegame`, `endgame`), `value` |
| `[%cbpawns …]` | `14` pawn structure | `value` |
| `[%cbpath …]` | `15` piece path | none |
| `[%cbcolour …]` | `23` variation colour | none |
| `[%cblink …]` | `1c` web link | `url`, `caption` |
| `[%cbvideo …]` | `20` video | `language` (a number), `caption` |
| `[%cbtraining …]` | `09` training question | `variant`, `seconds` (left out when negative), `points` |
| `[%cbtimecontrol …]` | `24` time control, 2CBH | per stage `A`, `B` and `C`, a stage all zero left out: `kindA` (0 the rest of the game, 1 a stage of `movesA` moves, 3 the rest of the game with an increment, 5 no time, 2 unknown), `initialA` and `incrementA` in seconds, `movesA` (1000 for the rest of the game); likewise `…B` and `…C`. A record holding a negative time is written as `[%cbraw]` |
| `[%cbraw type=<hex>;data=…]` | any other type, and in a classic database every type but texts, symbols, squares, arrows, quotations and medals, whose layouts there are not decoded | `type`, two hex digits |
| `[%cbrest type=<hex>;data=…]` | the bytes of the record after a type of unknown layout, in the game comment; the game is `incomplete` | `type` |

These types are `[%cbraw …]`, their data kept whole:
- evaluations (`26`) and engine evaluations (`21`): they are also written as
  `[%evp]` and `[%eval]` above;
- time spent (`07`): also written as `[%emt]`;
- the clocks (`16`, `17`): they hold one `int` per player for the game as a
  whole, in hundredths of a second, not a clock per move. ChessBase's own
  export of them is not available to confirm what they mean, so they are not
  written as `[%clk]`.

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

### `GET /v1/databases/{id}/explorer`

For a position: the games in the database that reached it in their first 40
plies, their results, the moves played from it, and its notable games. The
reference tab of the oschess analysis panel shows it like its Lichess tabs.

| Parameter | Meaning |
|---|---|
| `fen` | The position, in FEN; required |
| `variant` | `standard` (the default); any other value is answered `422 unsupported` |

```json
{
  "generation": "0a1b2c3d4e5f6071",
  "games": 5012345, "white": 1700000, "draws": 2100000, "black": 1212345,
  "moves": [ { "uci": "e2e4", "san": "e4", "games": 2305000, "white": 810000, "draws": 950000, "black": 545000 } ],
  "topGames": [ { "number": 1234, "white": "…", "black": "…", "whiteElo": 2882, "blackElo": 2800, "result": "1-0", "year": 2014, "event": "…" } ],
  "index": { "records": 11966514, "games": 11959813, "maxPly": 40 }
}
```

- **Counts.** `games` counts the games that reached the position; `white`,
  `draws` and `black` count those that ended so. A game without a result
  counts in `games` only. A game is counted once however often it reaches the
  position.
- **Moves** are the moves played from the position, most played first, with
  the same counts. Their `games` can add up to less than the position's: games
  that ended there, or reached it at the index's last ply, played no move from
  it within the index. `uci` writes castling the standard way, `e1g1` and
  `e1c1`; `san` is the move in SAN.
- **`topGames`**: up to 12 games that reached the position, the highest
  average rating first (the known rating when only one is), then the latest.
  Names are cut at 200 characters as in list rows; `year` is `null` when the
  date has none.
- **What is indexed.** The positions of each game's main line from its start to
  ply 40, and the moves played from them up to ply 40: standard chess only,
  from the standard start or a set-up position, without deleted games, guiding
  texts or analyses. A game whose move record is over 2 MiB, the limit
  `games/{number}` serves, or cannot be read, is left out. A position reached
  by only one game beyond ply 20 is left out. A position the index does not hold is answered with zero counts and
  empty lists. `index` says how far the index goes: the last record it covers,
  the games it holds, and its depth.
- **Chess960 is not indexed.** The Polyglot key the index uses names a
  castling right by its side, not by its rook, so two Chess960 positions that
  differ only in which rook may castle share a key. A FEN whose castling
  rights name rook files (Shredder-FEN, such as `4k3/8/8/8/8/8/8/4KR1R w F -`)
  or whose castling needs Chess960 rules is answered `422 unsupported` with
  `variant: "chess960"`, and so is `variant=chess960`.
- **Building.** The first request for a database's positions is answered from
  the index kept on disk when that index was built for the database as it is
  now (see **Storage**): opening it reads its header and block table, and waits
  for no build of another database. When the search memory has no room for
  that table, the request is answered `503 busy` and the next one tries again.
  Without such an index, the first request starts building it in the
  background: on at most half of the search workers, within
  half of the search memory budget, which it never takes from searches (it
  waits for memory searches hold), and one database at a time. It reads move
  records only, a few megabytes at a time, never annotations. The notable
  games rendered for answers are kept for all databases together, within the
  budget (a 64th of it, at most 8 MiB), and searches that need the memory
  drop them. Until the index is ready, requests are answered
  `409 database_unavailable` with `state: "indexing"` and
  `progress: {"phase", "done", "total"}`, and only while a build runs or waits
  to run; the phases are `checking` (records), `reading` (records) and
  `merging` (entries). `/v1/status` lists the builds
  under `indexing`. A build that fails is answered `503 index_unavailable` for
  a minute, and the next request tries again.
- **Changes.** The index belongs to the database's generation, and a change to
  the database rebuilds its index, about 5 minutes for the Mega Database.
  Until the new index is ready the answer is `409` with `state: "indexing"`:
  an index is never answered for another generation than its own. Updating an
  index from the games appended to a database is a possible later
  optimisation, only if its results can be shown equal to a build from
  nothing.
- **Storage.** Index files live in the data folder's `index` folder, one per
  database (`<id>.idx`); a PGN file's header index lives apart, in `pgn`
  ([PGN files](#pgn-files)). `docs/format-notes.md`,
  "Position index", describes them. A file that is damaged or of another
  version is rebuilt. The Mega Database's index takes about 1.4 GB, and its
  build needs about 8 GB of temporary space there (`<id>.build`,
  `<id>.idx.partial`). When the bridge starts, after each change of the
  database list, and at least once a minute while the list is asked for, the
  folder is swept (#60):
  - a build's leftovers go at once unless that build is running;
  - the index of a database that has been off the list for ten minutes goes.
    A database that is only missing, in the cloud or downloading is still on
    the list and keeps its index. The wait keeps a list that loses a database
    for a moment, as while ChessBase rewrites it, from costing a rebuild.

  Nothing else in the folder is touched.

### `GET /v1/engine/analyze`

The engine's lines for one position, streamed while it searches (#13). The
engine is the one `bridge.toml` names:

```toml
engine = 'C:\Program Files\Stockfish\stockfish.exe'
engine_threads = 6   # optional default; all processors but two without it
engine_hash = 512    # optional default, in MB; 512 without it, at most a quarter of the memory
```

`bridge.toml` is followed while the bridge runs, as the database list follows
it: a changed engine takes effect at once. A file that cannot be read or
parsed leaves the settings last read in force, for the databases and the
engine alike, and is read again at the next look; no file at all is the
defaults, no engine.

| Parameter | |
|---|---|
| `fen` | The position, at most 128 bytes; the start position without it. It must be a legal standard position; Chess960 is not analysed. |
| `moves` | UCI moves played from it, separated by spaces, at most 600. Castling is the king's two-square step (`e1g1`); the king taking its rook is accepted too. Every move must be legal. |
| `multipv` | Lines to search, 1 to 5; 1 by default. |
| `depth` | Search to this depth, 1 to 99, then name the best move. |
| `movetime` | Search this many milliseconds, 1 to 600000, then name the best move. Only one of `depth` and `movetime`. |
| `stream` | The client's view, at most 64 letters, digits, `-` and `_`, for example one browser tab. |
| `threads` | The engine's `Threads`, 1 to `engine.threads.max` of `GET /v1/status`; the default there without it. |
| `hash` | The engine's `Hash` in MB, 16 to `engine.hash.max`; the default there without it. |

Nothing else from the browser reaches the engine: the position goes to it as
`chesscore` writes it after checking it, and `Threads` and `Hash` are the
numbers above, sent only when they differ from the engine's current ones;
Stockfish takes both between searches, and a new `Hash` clears its table. A parameter out of bounds answers `400 bad_request` naming it;
without an engine the answer is `409 no_engine`.

The answer is `200` with `Content-Type: application/x-ndjson` and a chunked
body: one JSON object per line, until the search ends.

```
{"info":{"depth":24,"seldepth":33,"multipv":1,"score":{"cp":31},"nodes":5210034,"nps":2605017,"time":2000,"pv":["e2e4","e7e5","g1f3"]}}
{"info":{"depth":24,"seldepth":30,"multipv":2,"score":{"mate":-7},"bound":"upper","nodes":5210034,"nps":2605017,"time":2000,"pv":["d2d4","d7d5"]}}
{"bestmove":"e2e4"}
```

- `info` is the newest line of each `multipv` number, written at most every
  250 ms. Scores are from the side to move, in centipawns (`cp`) or moves to
  mate (`mate`, negative when the side to move is mated). `bound` is `lower`
  or `upper` when the score is only a bound. `seldepth`, `nodes`, `nps` and
  `time` appear when the engine gave them. In a position already decided,
  mate or stalemate on the board, the engine gives a depth 0 score with an
  empty `pv`, and the best move is `(none)`.
- With nothing new for two seconds, the last lines are written again, so a
  long step of the search keeps the connection alive.
- `bestmove` ends a search with `depth` or `movetime`. Without either, the
  search goes on until the client closes the connection, which stops it.
- `{"superseded":true}` ends the search when a newer request with another
  `stream` took the engine. A newer request with the same `stream` ends it
  without that line. The engine searches one position at a time.
- `{"error":{"code":"engine_exited","message":…}}` ends it when the engine
  stopped; the next request starts it again. `engine_failed` means it could
  not be started or is not a UCI engine.

`EventSource` cannot send the `Authorization` header, so a client reads the
body from `fetch` as a stream. The engine runs at below-normal priority, starts
with the first request and ends after ten minutes without one.

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
