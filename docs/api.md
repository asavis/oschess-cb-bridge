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
- HTTP/1.1. `GET` and `OPTIONS`, and the writes of a PGN file's games:
  `POST`, `PUT` and `DELETE` ([Writing games](#writing-games)). Only the
  writes of a game carry a body, sent with `Content-Length`.
- Every response body is JSON, UTF-8, `Content-Type: application/json;
  charset=utf-8`. Clients must ignore fields they do not know; see
  [Compatibility](#compatibility).
- Limits: the request line and headers together at most 16 KiB; at most 32
  open connections; an idle connection is closed after 5 seconds and a
  request that has not arrived completely after 10 seconds is dropped.

## Compatibility

Version 1 changes only in ways a client written against an earlier text of
this document keeps working with. The bridge and its clients hold to four
rules:

- **The bridge sends what it decoded.** A record is sent whole wherever an
  answer names it: every game in an answer is a row of
  `GET /v1/databases/{id}/games`, with all of that row's fields; an endpoint
  may add fields to it but never leaves one out. Values are in the forms this
  document defines (PGN text for dates, results, rounds and ECO codes,
  `Last, First` for names, numbers as numbers), never localised, abbreviated
  or chosen for one screen. The documented bounds, such as 200 characters for
  a text field, are the only cuts.
- **Fields are only added.** A field keeps its name, type and meaning within
  version 1, and a bridge that once sent it goes on sending it. A change that
  removes or reinterprets a field is version 2, served under `/v2`.
- **Clients tolerate what is missing or new.** A client ignores fields and
  values it does not know, a new indexing phase among them, and treats a
  documented field that is absent as unknown: the bridge is older than the
  field. An older bridge ignores a parameter it does not know, so every
  parameter that narrows an answer is acknowledged in it (as `line` is, in
  each row); a client that does not find the acknowledgement treats the answer
  as not narrowed.
- **Bounded parameters, not constants.** Where an answer holds a number of
  items or plies that a screen may want more or fewer of, the number is a
  parameter with a default and a range given here, not a constant of the
  bridge; a value out of range is `400 bad_request`. The explorer's 12
  notable games are a bound the position index itself sets.

## Access

Every request other than an `OPTIONS` preflight must pass all of these checks.
A request that fails one gets the status and error code listed, and learns
nothing about the databases.

| Check | Failure |
|---|---|
| `Host` is `127.0.0.1:<port>`, `localhost:<port>` or `[::1]:<port>` (guards against DNS rebinding) | `421 misdirected_host` |
| `Origin`, when present, is on the allowlist | `403 forbidden_origin` |
| `Authorization: Bearer <token>` carries the pairing token | `401 unauthorized` |
| The method is served at the path: `GET` at every path, `POST` at `/v1/databases/{id}/games`, `PUT` and `DELETE` at `/v1/databases/{id}/games/{number}` | `405 method_not_allowed`, with `Allow` naming the methods served there |
| No body (`Content-Length` absent or 0, no `Transfer-Encoding`), except on `POST` of `/v1/databases/{id}/games` and `PUT` of `/v1/databases/{id}/games/{number}`, whose body is sent with `Content-Length` | `413 body_not_allowed` |
| Request line and headers within 16 KiB | `431 headers_too_large` |

The size of the request line and headers is checked first, as they arrive;
then the others, in the table's order. A body is read only once all of them
pass.

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
Access-Control-Allow-Methods: GET, POST, PUT, DELETE
Access-Control-Allow-Headers: Authorization, Content-Type, If-Match
Access-Control-Max-Age: 600
Vary: Origin
```

and, when the preflight carries `Access-Control-Request-Private-Network: true`,
also `Access-Control-Allow-Private-Network: true`. A preflight from any other
origin is answered `403 forbidden_origin` without CORS headers. Every other
response to an allowed origin, errors included, carries

```
Access-Control-Allow-Origin: <the request's Origin>
Access-Control-Expose-Headers: Retry-After, ETag
Vary: Origin
```

so that the page can read the error body, the retry delay and the generation
a write answers with.

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
| 400 | `bad_request` | A parameter is missing or malformed; `parameter` names it. A write's body that is not one playable game is `parameter: "body"` ([Writing games](#writing-games)) |
| 400 | `query_syntax` | Reserved: the search grammar is lenient and no text is a syntax error today |
| 400 | `unsupported_qualifier` | `q` uses a qualifier ChessBase databases do not have; `qualifier` names it |
| 401 | `unauthorized` | Token missing or wrong |
| 403 | `forbidden_origin` | `Origin` not on the allowlist |
| 404 | `not_found` | No such path, database or game number |
| 405 | `method_not_allowed` | The method is not served at the path; `Allow` names those that are |
| 409 | `database_unavailable` | The database is not `ready`; `state` gives its state. A request for the games of a `cloudOnly` database starts its download and is answered with `downloading`. For the explorer and the games of a position or of a fragment, `state: "indexing"` with `progress` while the position index is built or waits to be built, or a fragment search's masks are built |
| 409 | `generation_changed` | A write names a generation the file no longer has: it changed since the client read it, through ChessBase, another program, a sync client or another write. Nothing was written |
| 409 | `read_only` | A write to a database that takes none: a 2CBH or classic database, or a PGN file with the read-only attribute |
| 409 | `file_busy` | Windows refused the write because another program holds the file, as ChessBase holds a database it has open. Nothing was written; the bridge neither waits nor retries |
| 409 | `superseded` | A newer search (`q`, `fen` or a fragment) on the same database replaced this one while it ran; the page shows the newer answer |
| 413 | `body_not_allowed` | The request has a body where none is taken, or sends one in chunks |
| 413 | `body_too_large` | A write's body is over 4 MiB |
| 421 | `misdirected_host` | `Host` is not a loopback name |
| 422 | `database_too_large` | Searching or sorting this database needs more than the whole search memory budget; number order still works |
| 422 | `unsupported` | The position or variant of the explorer, of `fen` or of a fragment is Chess960, which the position index does not hold; `variant` names it |
| 422 | `not_a_game` | The record is a guiding text or an analysis, which the bridge does not serve as PGN |
| 422 | `games_would_join` | A write would join games: the new game to a neighbour, or the games on either side of one removed, because a game next to it lacks its result or holds tags alone. Nothing was written |
| 422 | `unencodable` | The game of a write holds a character that the file's code page cannot store; `character` names the first. Nothing was written |
| 422 | `unreadable_game` | The game's records are damaged and stay so between reads, or it is too large to serve (a move or annotation record over 2 MiB, or an answer over 8 MiB); `reason` says which, in English |
| 428 | `precondition_required` | A write without `If-Match` |
| 431 | `headers_too_large` | Request line and headers over 16 KiB |
| 500 | `write_failed` | The file system refused a write for another reason than another program holding the file, a full disk among them. The file is as it was; the bridge logs why |
| 500 | `internal` | A bug; the bridge logs it with the database's `id`, in `bridge.log` in its data folder and, in a console, on standard error |
| 503 | `database_changing` | ChessBase changed the database during the read; `Retry-After: 1` |
| 503 | `index_unavailable` | The position index, or the masks of a fragment search, could not be built; the message says why, too little free disk space among the reasons, and the next request after a minute tries again |
| 503 | `busy` | Too many open connections, too many large answers being sent at once, or search memory taken by other searches; `Retry-After: 1` |

## Database identity and generations

- A database's `id` is 16 lowercase hexadecimal characters derived from its
  normalised path. It is stable while the path stays the same. The path itself
  is never sent.
- A database's `generation` is an opaque string that changes whenever the bridge
  sees the database's files change: their sizes, their modification times, or
  the files themselves, as when one is replaced by a copy (#241). Responses
  that depend on the contents carry the generation they were read at. A client
  that sees it change in the middle of paging through a list starts the list
  again. The files are those the bridge reads: `.2cbh`, `.2cbg`, `.2cba`,
  `.2lid`, `.2lgd` and `.2lcd` of a 2CBH database; `.cbh`, `.cbg`, `.cba`, `.cbp`,
  `.cbt`, `.cbc`, `.cbs` and, when present, `.cbj` of a classic one (see
  [Classic databases](#classic-databases)); a PGN file itself (see
  [PGN files](#pgn-files)). For a PGN file, a bridge whose reading of PGN
  files changed also reports a new generation.
- A write into a PGN file names the generation its client read and answers
  with the new one ([Writing games](#writing-games)).

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
- **Between reads, the bridge holds no file of a database open** (#241).
  ChessBase opens the files of a database it saves so that no other program
  may hold them, and a save fails with its "Save Error" while any other
  program does. So on Windows, the bridge opens a database's files only to
  read them, and closes them about a quarter of a second after its last read.
  A save into a database fails while the bridge reads it, as for a list, a
  search, an index build or games, and for that quarter of a second after.
  Once the bridge is done with it, the save succeeds when made again. A read
  that meets a file ChessBase holds fails at once, as a read of an unreadable
  file does. A file opened again must be the one the database was opened on,
  as it was then: the same volume, file id, size and time of last change. A
  file that changed or was replaced since then is not read. The answer is
  then the one for a read during a change. The database's generation changed
  with the file, so a retry reads the database as it is now. This needs file ids that no other file takes while the file
  lasts, which NTFS and ReFS give. On another file system, such as FAT, and on
  Linux and macOS, a file opened again at its path could be another file.
  There, the bridge keeps each file of an open database open, as every
  version before did.
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
The position index's move stream is not in the budget: it is mapped
read-only, and its pages are the operating system's file cache, which drops
them when memory is short and reads them again when they are needed. The games
of a position that a list is narrowed to (`fen`) are in the budget: one bit
per record while they are found, and 4 bytes a game in the list kept for
paging.

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
example one per browser tab and list. A request that carries `q` or `fen` and
a `stream` supersedes the search still running in the same stream on the same
database: that one stops at its next batch of headers, or of the games of a
position it looks through, and is answered `409 superseded`. An empty `q=`
counts: clearing the search box supersedes the search it replaces. Other
streams, requests without a `stream`, requests without `q` or `fen`, and
suggestions are never superseded, so a page in one tab never stops a search
another tab still waits for. A database remembers its 256 most recently used
streams; a forgotten stream starts afresh.

## Endpoints

### `GET /v1/status`

```json
{
  "bridge": { "version": "0.4.0", "api": 1, "features": ["explorerSearch", "fragmentSearch"] },
  "databases": { "ready": 9, "opening": 0, "missing": 1, "cloudOnly": 1, "downloading": 1, "unsupported": 2, "unreadable": 0 },
  "download": { "present": 104857600, "total": 734003200 },
  "indexing": [ { "id": "0a1b2c3d4e5f6071", "phase": "reading", "done": 4000000, "total": 11966514 } ],
  "engine": { "name": "Stockfish 19", "threads": { "default": 6, "max": 8 }, "hash": { "default": 512, "max": 8192 } }
}
```

The web app calls it first. `api` below the version it was written for means
the bridge is too old; the app then offers the download link. `features`
names the optional features of version 1 this bridge has, each once (#270):
`explorerSearch`, the explorer's `q` and its `filter` acknowledgement (#268);
`fragmentSearch`, the games of a position fragment and of material, with
their `fragment` acknowledgement (#272, [Games of a fragment](#games-of-a-fragment)).
A client offers a feature only when it finds it named. A bridge older than
the list sends no `features`, which a client reads as an empty list, and a
client ignores names it does not know (Compatibility, rule 2: a field only
added; a later feature adds its name). `databases`
counts the databases in each state; `opening` counts the PGN files being read
for their header index (see [PGN files](#pgn-files)). `download` is there while databases are
being downloaded: the bytes on this computer and in all, over all of them.
`indexing` is there while position indexes are checked, built or waiting to
be built (see `GET /v1/databases/{id}/explorer`). Each has the database
`id`, its `phase`, and `done` of `total`. The phases are `waiting` (queued
behind another build), `checking` (records), `reading` (records),
`positions` (the tree's entries) and `structures` (the deep section's
postings), and `masks` while the masks of a database's games are built for a
search by a fragment ([Games of a fragment](#games-of-a-fragment)). A client
shows a phase it does not know as it shows these.
`engine` names the engine the analysis
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
      "writable": false,
      "records": 11966514,
      "generation": "g1b2c3d4",
      "folder": ["Bases", "Mega2026"],
      "created": "2025-11-20T09:14:03Z",
      "modified": "2025-11-20T09:31:47Z"
    },
    {
      "id": "5e4f30219a8b7c6d",
      "name": "Club 2025",
      "format": "2cbh",
      "state": "downloading",
      "writable": false,
      "size": 734003200,
      "progress": { "present": 104857600, "total": 734003200 },
      "folder": ["MyWork", "Club"],
      "created": "2025-07-07T16:02:11Z",
      "modified": "2025-08-29T19:45:00Z"
    },
    {
      "id": "9a8b7c6d5e4f3021",
      "name": "Openings",
      "format": "pgn",
      "state": "opening",
      "writable": false,
      "progress": { "present": 52428800, "total": 157286400 },
      "folder": ["D:", "Chess", "TWIC"],
      "created": "2026-09-29T07:30:12Z",
      "modified": "2026-09-29T07:30:12Z"
    }
  ]
}
```

| Field | Meaning |
|---|---|
| `name` | The name ChessBase's window shows: the title it keeps for the database, else the file name without extension. A database that is not in the window has its file name without extension |
| `format` | `2cbh`, `cbh` or `pgn`; another value is possible later |
| `state` | `ready`; `opening` (a PGN file being read for its header index, see [PGN files](#pgn-files); retry after a moment); `missing` (the file is gone, or the database left the list); `cloudOnly` (kept only in the cloud, not on this computer; see below); `downloading` (being brought to this computer; see below); `unsupported` (a format the bridge does not serve: any but `.2cbh`, `.cbh` and `.pgn`); `unreadable` (the files are present but cannot be opened: damaged, locked by another program, or not regular files, such as a folder or a pipe named like one; for a PGN file, its header index could not be built) |
| `writable` | Whether the database takes writes now ([Writing games](#writing-games)): `true` for a `ready` PGN file without the read-only attribute, else `false`. A client offers writes only where it is `true`; a row without it comes from an older bridge, whose databases take none |
| `records` | Games, guiding texts and analyses; present when `ready` |
| `generation` | See above; present when `ready` |
| `size` | The bytes of the database's files; present when `cloudOnly` or `downloading` |
| `progress` | `present` bytes on this computer of `total`, when `downloading`; `present` bytes of the PGN file read of `total`, when `opening` |
| `folder` | The folder holding the database, as an array of path segments, for a client to show and search (#298). Inside ChessBase's documents folder (`Documents\ChessBase`) it is relative to it: `["Bases", "Mega2026"]`, and `[]` for a database directly in it. Elsewhere under the user's profile folder it starts with the segment `"~"` for that folder: `["~", "Desktop", "Chess"]`. Elsewhere it is the whole path from its drive: `["D:", "Chess", "TWIC"]`, or `["\\\\nas\\share", "Bases"]` on a network share. The path is first made absolute and its `.` and `..` resolved, by its spelling alone, and a drive or share is named plainly however it is written (`\\?\C:\…` is `C:`, `\\?\UNC\nas\share` is `\\nas\share`), so every spelling of a folder names it alike; the user's name never appears. Always present |
| `created` | When the database was created: the creation time of its main file (`.2cbh`, `.cbh` or `.pgn`; for another format, its file), in UTC, RFC 3339 to the second. Absent when `missing`, when the file system keeps no creation time, and for a time outside the years 0000 to 9999, which RFC 3339 cannot write |
| `modified` | When the database last changed: the latest modification time of the files it is read through (for 2CBH `.2cbh`, `.2cbg`, `.2lid`, and `.2cba` when present, never its `.2lgd` and `.2lcd`; a game added in ChessBase writes `.2cbg` alone), in the form of `created`. Absent when `missing`, and outside the years 0000 to 9999 |

`folder`, `created` and `modified` come from the files' metadata: a cloud-only
database's times are read without opening its files, so listing still
downloads nothing. A row without them comes from an older bridge.

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
database `cloudOnly`; the bridge logs that, with the database's `id`, in
`bridge.log` in its data folder and, in a console, on standard error. It
downloads again only when the database's games are requested again.

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
- **Text that is not UTF-8**, in names and comments, is read in
  Windows-1251 or Windows-1252, as its words show, on a computer whose ANSI
  code page is one of the two, and ChessBase's piece bytes in it are the
  figurines ♔ ♕ ♘ ♗ ♖ ♙; on a computer of another page it is read in that
  page ([format notes](format-notes.md), "Russian databases hold
  Windows-1251").
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
  request tries again. A write of the bridge's own does not read the file
  again: it makes the header index of the new file from the one before
  ([Writing games](#writing-games)). A header index takes 48 bytes a game
  plus the names, among them the game's time control, which its
  `TimeControl` tag gives (#268,
  [search-grammar.md](search-grammar.md#time-control)).
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
  `lang` changes nothing, and `annotations` is `complete`: a PGN game's
  comments are all it has. The reading form reads the layout marks that a
  Chessable course's export writes in comments, words between runs of two
  `@` or more, as a 2CBH copy of the course shows them (#318):
  `@@StartBracket@@39@@EndBracket@@` is `(39)`, `StartSquare`/`EndSquare`
  are `[` `]`, `StartFEN`/`EndFEN` set the FEN apart as `[FEN …]`, and
  `LinkStart`/`LinkEnd` leave the address alone. The full form serves the
  text as the file writes it, and a write keeps the file's text.
- **Position index.** Each game's main line is played as written, from its
  `FEN` tag or the standard start, and ends at the first move that names no
  legal move, a null move among them. Games of Chess960 and of other variants
  are left out.
- **`line`.** A row's `line` is the main line read the same way, written in
  the bridge's SAN, so its form may differ from the text's: `O-O` for `0-0`,
  `e8=Q` for `e8Q`. A game with a `FEN` tag other than the standard position
  has none.

### `GET /v1/databases/{id}/games`

One window of the database's records, sorted and optionally searched or
narrowed to the games of a position or of a position fragment.

| Parameter | Default | Meaning |
|---|---|---|
| `offset` | `0` | The first row of the window, counted from 0 in the sorted order |
| `limit` | `200` | Rows in the window, 1 to 500 |
| `sort` | `number` | `<key>`, `<key>-asc` or `<key>-desc`; keys below. It wins over a `sort:` token in `q`; an unknown key is `400 bad_request` |
| `q` | | A search in the Library search grammar ([search-grammar.md](search-grammar.md)); `total` then counts the matches |
| `stream` | | The client's name for this list, which lets a newer search replace an older one ([Cancellation](#cancellation)); an invalid name is `400 bad_request` |
| `line` | | Plies of each game's main line to add to its row, 1 to 60 (below); any other value is `400 bad_request`. Without it, rows are as shown |
| `fen` | | A position in FEN: the window holds only the games whose main line reaches it, at any ply ([Games of a position](#games-of-a-position)); `total` counts them |
| `variant` | `standard` | With `fen` or a fragment: any value other than `standard` is `422 unsupported` |
| `look`, `nowhite`, `noblack`, `or`, `exclude`, `material`, `mirror`, `first`, `last`, `length` | | A position fragment and material: the window holds only the games whose main line holds them ([Games of a fragment](#games-of-a-fragment)); `total` counts them |

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
| `total` | Rows matching `q` and `fen` or a fragment (all records without any) |
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

#### Games of a position

With `fen`, the list holds the games the explorer counts for that position
(`GET /v1/databases/{id}/explorer`): each game whose main line reaches it at
any ply, once; standard chess only, from the standard start or a set-up
position; never a deleted game, a guiding text or an analysis. `q` narrows
them further, a game matching both; `sort`, `offset`, `limit`, `stream` and
`line` apply as without `fen`, and `total` counts the games that match both.

The answer acknowledges the position:

    "position": { "fen": "rnbqkbnr/pp1ppppp/8/2p5/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 2", "games": 1234567 }

`fen` is the position as the bridge writes it; `games` counts its games
before `q`, the explorer's `games`. A bridge older than this parameter
ignores it and answers the whole list without `position`: a client that sent
`fen` and finds no `position` treats the list as not filtered.

The games of a position come from the position index. Until it is ready the
request is answered as the explorer's is: `409 database_unavailable` with
`state: "indexing"` and `progress`, or `503 index_unavailable` when it could
not be built. A FEN that is not a valid position is `400 bad_request` naming
`fen`; a Chess960 position, or a `variant` other than `standard`, is
`422 unsupported` with `variant: "chess960"`. A position no game reaches has
`total: 0` and no rows.

The first window of a position reached by more than twelve games within its
first 20 plies replays the first plies of the games that can reach it. On the
Mega Database a first window of 200 rows sorted by date took about 65 ms for
the start position (11,957,416 games) and 25-30 ms at plies 6 to 20, and a
position beyond ply 20 about 4 ms. The first such list after the bridge opens
the index also checks those first plies against their CRCs, once: some 8 ms
more for 12 million generated games on 16 threads. The result is kept with the
latest searches, so the next windows of the same position, sort and `q` are
read from it, in about 2 ms.

#### Games of a fragment

A position fragment and material find games by what stands where, as the
Position and Material tabs of ChessBase's game filter do (#272): the games
whose main line holds them, at some ply, each game once. Every parameter is
optional, but `mirror`, `first`, `last` and `length` apply only to one of the
others.

| Parameter | Meaning |
|---|---|
| `look` | Pieces that all stand on their squares: `Nd5,pd6`, a piece's letter as FEN writes it (white in upper case, `K Q R B N P`) and its square, comma-separated, 32 at most |
| `nowhite` | Squares no white piece stands on: `c2,c4`. With `noblack` on the same square, the square is empty. ChessBase's white and black points |
| `noblack` | Squares no black piece stands on |
| `or` | Pieces of which at least one stands on its square, written as `look` |
| `exclude` | Pieces none of which stands on its square, written as `look`; at most four on one square |
| `material` | Counts of each side's kinds: `Q0,q0,R1..2,p..4`, a kind's letter (no king) and a count, or a range `a..b` with either end open, from 0 to 16; a kind named once at most. A kind left out is any count |
| `mirror` | `none` (the default); `horizontal`, the fragment flipped a↔h as well; `vertical`, flipped rank 1↔8 with the colours changed as well, so `Bh7` finds a white bishop on h7 or a black one on h2; `both`, all four forms. Material is never mirrored |
| `first`, `last` | Move numbers from 1 to 999 (default 1 and 999): a position counts only from move `first` to move `last`, the move number being the one its side to move plays next. A set-up game's moves are numbered from its start's move number, as the database stores it |
| `length` | Plies, 1 to 99 (default 1): the fragment and the material must hold for that many positions in a row |

A game matches when its main line, from its start, holds the fragment, in
one of its forms, and the material, for `length` consecutive positions whose
move numbers lie from `first` to `last`. A position holds the fragment when
every piece of `look` stands on its square, no white piece on a `nowhite`
square and no black one on a `noblack` square, no piece of `exclude` on its
square, and, when `or` names pieces, one of them on its square. As with
`fen`, the games are standard chess only, from the standard start or a set-up
position, never deleted games, guiding texts or analyses, and only main
lines: a variation is not searched.

Each row gains one more field, the first match:

| Field | Meaning |
|---|---|
| `match.ply` | The first position of the first such stretch: 0 is the game's start position, `n` the position after its `n`th move, so a client opens the game there |

    "match": { "ply": 24 }

`q` narrows the games further, a game matching both. `sort`, `offset`,
`limit`, `stream` and `line` apply as without a fragment, and `total` counts
the games that match. `fen` together with a fragment or material is
`400 bad_request` naming `fen`.

The answer acknowledges the filter as `position` acknowledges `fen`, every
parameter in one form, the fragment's pieces ordered by colour, kind and
square:

    "fragment": { "look": "Nd5,pd6", "nowhite": "", "noblack": "", "or": "", "exclude": "", "material": "", "mirror": "horizontal", "first": 1, "last": 999, "length": 1, "games": 4182 }

`games` counts the games that match before `q`. A bridge without this search
ignores its parameters and answers the whole list without `fragment`: a
client that sent a fragment and finds no `fragment` treats the list as not
filtered. `/v1/status` names the search among its features,
`fragmentSearch`, and a client offers it only then.

A parameter that cannot be read is `400 bad_request` naming it: a piece or a
square that is not one, more than 32 pieces on a board or four on an
Exclude square, an empty or doubled material range, a mirror, move number or
length out of its bounds, `first` after `last`, or `mirror`, `first`, `last`
or `length` without a fragment or material. A `variant` other than `standard`
is `422 unsupported`.

The search needs the position index, and the index's masks of the games:
for each game, the squares each side's pawns, minor pieces and major pieces
stood on and the least and the most of each kind it had, at any ply. A game
whose masks lack what the filter needs cannot match, and is never replayed;
the others are replayed from the index's move stream. The first search by a
fragment on a database starts the masks' build, as ChessBase's search
booster: until the index and then the masks are ready, the request is
answered `409 database_unavailable` with `state: "indexing"` and `progress`,
whose `phase` is `masks` while they are built, and `/v1/status` lists the
build under `indexing`. The masks are kept beside the index, 64 bytes a
record, and belong to its build: a change to the database, which builds the
index again, makes them stale, and a database that has masks has them built
again right after its new index, before a search asks for them. A build
that fails is answered `503 index_unavailable` for a minute, and the next
request tries again. A database's own `.cbb` is never read.

The result is kept with the latest searches, so the next windows of the same
filter, sort and `q` are read from it; `match.ply` is found again for the
window's games alone. A newer search in the same `stream` replaces a running
one ([Cancellation](#cancellation)).

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

### Writing games

A PGN file takes writes (#281): a game appended at its end, or one replaced
or removed. Nothing else is ever written: ChessBase's own formats (`2cbh`,
`cbh`) answer every write `409 read_only`, and so does a PGN file with the
read-only attribute. A database row says whether it takes writes now with
`writable` (see `GET /v1/databases`). The bridge writes only the file behind
a listed `id`: it never takes a path from a request, and never makes, renames
or removes a file of its own accord, but for its temporary file below.

| Request | Does | Answer |
|---|---|---|
| `POST /v1/databases/{id}/games` | Appends the body's game at the end of the file | `201`, `{ "number": 13, "generation": "…" }`: the new game's number and the new generation |
| `PUT /v1/databases/{id}/games/{number}` | Replaces game `number` with the body's game | `200`, `{ "number": 4, "generation": "…" }` |
| `DELETE /v1/databases/{id}/games/{number}` | Removes game `number`; the games after it move up by one | `200`, `{ "generation": "…" }` |

Every answer also carries the new generation as its `ETag`, quoted as an
entity tag (`"0123456789abcdef"`).

- **The body.** `POST` and `PUT` carry one game's PGN text, UTF-8, as
  `Content-Type: application/x-chess-pgn`, at most 4 MiB, the oschess
  Library's bound for PGN; a larger one is `413 body_too_large`. The bridge
  reads it with its own PGN reader, as it reads a file ([PGN files](#pgn-files)),
  and writes the game from its first tag (or move) to its end: blank lines and
  text outside the game are not the game's. A body is `400 bad_request` with
  `parameter: "body"` when it is not UTF-8, holds no game or more than one,
  leaves a `{` comment open, holds what the reader passes over (a move,
  number or result over 16 characters, or bytes that make no PGN element
  outside comments and tags), or when its main line does not play. The main line plays when its `FEN`
  tag, if it has one, names a position (an empty one does not) and each of
  its moves is legal from there or from the standard start; a null move ends
  it, as in the position index. `DELETE` carries no body.
- **The precondition.** Every write names the generation its client read in
  `If-Match`, as the `ETag` gives it or bare. A write without one is
  `428 precondition_required`. When the file's generation is another, because
  ChessBase, another program, a sync client or another write changed it, the
  answer is `409 generation_changed` and nothing is written. The generation
  is told from the file's metadata, before the changed file is read: the
  answer is the conflict, never `opening`. The client reads
  the database again; the bridge never merges.
- **Which databases.** Only a PGN file in state `ready` takes writes: one that
  is not ready answers `409 database_unavailable` with its `state`, as a read
  does. A `number` the file does not have is `404 not_found`. A header index
  that claims more games than its file has bytes, as only a damaged one does,
  is removed and the file read again: the write answers
  `409 database_unavailable` with `state: "opening"` and writes nothing.
- **One at a time.** The bridge writes one write at a time, whatever
  database or path it names, so that two listed paths of one file, as a link
  and its target, are never written at once. A write that waits for another
  and names the generation before it is then `409 generation_changed`.
- **An append** writes after the file's last byte and leaves every earlier
  byte as it was. A file whose last line has no line end gets one, and then an
  empty line before the game unless the file ends with one already; the game
  is followed by an empty line, as PGN's export format ends each game. An
  empty file gets the game alone. When the write fails part-way, the file is
  cut back to its former length.
- **A replace or a removal** writes the whole new file beside the old one, as
  `<name>.pgn.oschess-tmp` in the same folder, flushes it to the disk, and
  renames it over the old one in one step: a crash leaves the old file or the
  new one, never a mix. The temporary file is made new: when a file or a link
  already has its name, the write is `500 write_failed` and that file is left
  as it is. Before any game goes into it, it takes the PGN file's access: its
  permissions and group on Unix (when the user may not give it the file's
  group and the file's mode grants that group or others any access, the
  write is `500 write_failed`: the group's members would be others), with its
  POSIX access control list on
  Linux, or none when the file has none, whatever the folder's default would
  give a new file. On Windows it takes the file's access control list. Kept
  from the folder's entries when the file's is, its entries inherited are the
  folder's, as the file's are. A list that cannot be given to it fails the
  write. So the
  new file is no more readable than the old one was. A temporary file a crash left is removed when the bridge next
  lists the file. A PGN path that is a link is written where it links to, and
  the link stays. A replace puts the game in the place of the old
  one's text, from its first tag to its end, and keeps the empty lines around
  it; where a neighbour shares the game's line, a line end keeps them apart.
  A removal takes the game and the empty lines after it, to the next game
  or the end of the file.
- **The neighbours stay as they were.** Before anything is written, the text
  from the start of the game before the edited one to the end of the game
  after it is read as it would be. A write goes on only when those two games
  read as they did and the new game reads as one game, exactly its text: the
  reader would otherwise join it to a neighbour that lacks its result or holds
  tags alone, or take the next game into it. A removal goes on only when the
  games on either side stay apart. Otherwise the answer is
  `422 games_would_join`, and nothing is written.
- **A file held elsewhere.** On Windows the bridge holds the file while it
  reads and writes it, so that no other program writes it meanwhile. When
  Windows refuses to open, write or replace the file because another program
  holds it, as ChessBase holds a database it has open, the answer is
  `409 file_busy` and nothing is written; the page then asks the user to close
  the database in ChessBase. The bridge neither waits nor forces anything.
  When the file system refuses for another reason, such as a full disk, the
  answer is `500 write_failed`, and the file is as it was.
- **Encoding and line ends follow the file.** The bridge writes in the
  encoding it reads the file in: UTF-8 when all of the file is UTF-8, which a
  file of pure ASCII is, otherwise the computer's ANSI code page, Windows-1252
  where the bridge has no table for it ([PGN files](#pgn-files)). The rest of
  the file is never encoded again. A game holding a character that the code
  page cannot store is `422 unencodable`, with `character` naming the first,
  and nothing is written. Line ends follow the file's first line end; a file
  without one gets CRLF, as ChessBase writes on Windows.

After a write:

- **The generation** changes, and the answer carries the new one.
- **The header index** of the new file is made from the one before: the
  games ahead of the changed one keep their entries, the text is read again
  from the game before it until the games are those of the old file again,
  and the entries after it move. The database stays `ready` and is not read
  again: the index is the one a reading of the whole file makes. A file whose
  text a write leaves inside a comment that is never closed is read again
  whole, within the write. A change made by another program still has the
  file read again, in the background, as before.
- **Sort orders, suggestions and the position index** follow the generation
  ([Consistency](#consistency)): the position index is built again in the
  background, as after any change.

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

For a position: the games in the database that reached it, at any ply, their
results, the moves played from it, and its notable games. The reference tab of
the oschess analysis panel shows it like its Lichess tabs.

| Parameter | Meaning |
|---|---|
| `fen` | The position, in FEN; required |
| `variant` | `standard` (the default); any other value is answered `422 unsupported` |
| `q` | A search in the Library search grammar ([search-grammar.md](search-grammar.md)): the answer counts only the games it selects (below) |

```json
{
  "generation": "0a1b2c3d4e5f6071",
  "games": 5012345, "white": 1700000, "draws": 2100000, "black": 1212345,
  "moves": [ { "uci": "e2e4", "san": "e4", "games": 2305000, "white": 810000, "draws": 950000, "black": 545000 } ],
  "topGames": [
    {
      "number": 1234, "kind": "game",
      "white": "…", "whiteElo": 2882, "black": "…", "blackElo": 2800,
      "result": "1-0", "moves": 41, "eco": "C65",
      "event": "…", "site": "…", "date": "2014.06.??", "round": "5", "annotator": "",
      "flags": { "deleted": false, "chess960": false },
      "year": 2014
    }
  ],
  "index": { "records": 11966514, "games": 11959813 }
}
```

With `q` the answer also acknowledges the search (#268):

    "filter": { "q": "tc:normal whiteelo:2200.. blackelo:2200..", "games": 5012345 }

- **Narrowed by a search** (#268). With a `q` that has a term, `games`,
  `white`, `draws`, `black`, `moves`, `topGames` and `featuredGames` count only the games of
  the position that `q` selects, the games `GET /v1/databases/{id}/games?fen=&q=`
  lists, by the rules below: each once, at the first ply its main line
  reaches the position, with the move it played from there; the notable games
  are the best rated of them. The answer acknowledges the search with
  `filter`: `q` as read, its first 1,024 characters, and `games`, the games of
  the position before it, the answer's `games` without `q`. A bridge older
  than this parameter ignores it and sends no `filter`: a client that sent
  `q` and finds none treats the answer as not narrowed.
  - `sort:` tokens are ignored, and a `q` with no term left, empty or only a
    sort, is the answer without it, with no `filter`.
  - A qualifier only the Library has is `400 unsupported_qualifier`, as in a
    list, before the database or its index is looked at: such a request
    starts no build. The other refusals, `409` while the index is built and
    `503`, are those of the answer without `q`.
  - The records `q` selects are found by one pass over the database's
    headers and kept as a set, a bit a record, with the latest four such
    searches: the next positions narrowed by the same `q` need no pass. The
    pass reads the database's heads file, whose build the explorer's first
    answer starts once the index is ready, and every header record until it
    is built. The position's games among them are replayed from the index's
    move stream to their first visit, on at most half of the search workers.
    The latest 64 narrowed answers are kept, within the search memory, so a
    position asked for again with the same `q`, as when a user steps back
    through a game, is answered without a replay.
  - Searches by `timecontrol:`, `whiteelo:`, `blackelo:` and `date:` narrow
    the reference to the games a player prepares from: normal games of
    rated players in a span of years, for example.
- **Counts.** `games` counts the games that reached the position; `white`,
  `draws` and `black` count those that ended so. A game without a result
  counts in `games` only. A game is counted once however often it reaches the
  position.
- **Moves** are the moves played from the position, most played first, with
  the same counts. Their `games` can add up to less than the position's: games
  that ended there played no move from it. `uci` writes castling the standard
  way, `e1g1` and `e1c1`; `san` is the move in SAN.
- **`topGames`**: up to 12 games that reached the position, the highest
  average rating first (the known rating when only one is), then descending
  record number. This legacy field keeps its original meaning.
  Each is a row of `GET /v1/databases/{id}/games`, with every field a row
  has, and `year`: the year of its `date`, `null` when the date has none.
  `year` stays for clients written before rows; `date` is the field to read.
- **`featuredGames`** (additive, API v1): up to 12 whole game rows, with
  `year` as in `topGames`, for clients that support database-relative selection.
  Use this list in its returned order when present, including an empty list;
  fall back to `topGames` only on older bridges that omit it. The heading can
  remain “Top games”; counts, moves and the All games list are unchanged.
  - Compute the mean of both source ratings, replacing each missing rating
    with 1500 for selection only. A half-point average stays exact. The rows
    still contain the source ratings, including zeros for unknown ratings.
  - Anchor the date windows to the newest valid game date in the database,
    before any user filter, including games outside the position index but
    excluding deleted records and non-game records. Unknown month/day means 1;
    an unknown year or invalid calendar date has no date. No known dates means
    every game enters the last tier.
  - First matching tier: average at least 2700 in the last calendar year;
    at least 2600 in the last three years; at least 2400 in the last five
    years; then everything else. Boundaries are inclusive. Calendar-year
    subtraction clamps February 29 to February 28 in a non-leap year.
  - Within each tier, descending average, then descending record number for
    equal averages. Date never breaks a tie. Take the first 12 of the combined
    order, with no tier quotas or duplicates, or every match if fewer than 12.
  - Indexing scans header batches once for the anchor, then writes each game's
    selection key into the move stream and precomputes the opening lists.
    Deep and filtered queries keep a bounded best-12 list using that key;
    they read no extra candidate headers and perform no full-result sort.
    Opening and deep selections merge under the same order. Only the selected
    rows are read for display (at most 24 across both API lists, sharing the
    existing rendered-row cache).
  - A generation change rebuilds both selections. Filtered-answer cache keys
    include build id, generation, position and search text, with at most 64
    answers within the existing memory budget. No daily recalculation occurs.
  Reproducible cost measurements: [selection-performance.md](selection-performance.md).
- **What is indexed.** Every position of each game's main line, to its end
  (its 65,535th ply at most): standard chess only, from the standard start or
  a set-up position, without deleted games, guiding texts or analyses. A game whose move record is over
  2 MiB, the limit `games/{number}` serves, or cannot be read, is left out.
  `index` names the last record the index covers and the games it holds.
- **How a position is found** (#133, #146, #147). The index has two parts:
  - **A tree** holds every position reached within the first 20 plies, each
    with its counts, moves and notable games. It counts each game that
    reaches such a position within those plies, however many share it.
  - **A deep section** holds, for every game, the structures its main line
    reaches beyond ply 20: a structure is each side's pawns and its pieces by
    kind, what only a pawn move or a capture changes. Neither is undone, so a
    game holds each structure for one stretch of plies, and few games share a
    deep one. The games of a position's structure are replayed on at most
    half the search workers: each game counts once, at the first ply its main
    line reaches the position, with the move it played from there. Fewer than
    1 in 100 of the Mega Database's positions share their structure with more
    than 4,096 games, and the most crowded structure, bare kings, with 38,367.

  The answer for a position is the tree's, and the games of its structure
  that the tree did not count: all of them when the tree does not hold the
  position, else those that first reach it beyond ply 20. A game that reaches
  it beyond ply 20 holds its structure there, so no game is missed, and a game
  that reaches it within ply 20 is the tree's, however often it comes back, so
  none is counted twice. Their counts add, their moves add, and the notable
  games are the best of both.

  The games are replayed from the index's **move stream** (#145), which holds
  every game's main line as the move codes of the 2CBH format, checked when
  the index was built, so a replay reads none of the database's files and
  checks no move. A replay stops as soon as its game can no longer reach the
  position, having fewer men or pawns of a side, or no longer having a pawn
  on a home square where the position has one; a game whose home pawns left
  in an order the position excludes is not replayed.

  A position no game reaches is answered with zero counts and empty lists.
- **Chess960 is not indexed.** The Polyglot key the index uses names a
  castling right by its side, not by its rook, so two Chess960 positions that
  differ only in which rook may castle share a key. A FEN whose castling
  rights name rook files (Shredder-FEN, such as `4k3/8/8/8/8/8/8/4KR1R w F -`)
  or whose castling needs Chess960 rules is answered `422 unsupported` with
  `variant: "chess960"`, and so is `variant=chess960`.
- **Building** (#149). The first request for a database's positions is
  answered from the index kept on disk when that index was built for the
  database as it is now (see **Storage**): opening it reads its header and
  block table, and waits for no build of another database. When the search
  memory has no room for that table, the request is answered `503 busy` and
  the next one tries again. Otherwise the index is built in the background,
  one database at a time, on at most half of the search workers and within
  half of the search memory budget, which it never takes from searches (while
  searches hold memory, a build takes less and runs more passes, and it waits
  for the least it needs).
  - A database's index is built when its explorer or the games of one of its
    positions are asked for. That build goes first, at below-normal priority,
    and stops a background build of another database at its next batch,
    which waits its turn again.
  - It is also built, unasked, for each database in use whose index is not
    of the database as it is now: once built, whenever the database changes.
    A database is in use when its index is kept on disk, or its explorer or
    the games of one of its positions were asked for since the bridge
    started; the database with the most records is in use from the start. A
    list of a database's games alone does not put it in use. Such a build
    starts once the database has not changed for a minute, never while it is
    kept only in the cloud, downloading, or a PGN file being opened, and on
    Windows not while the computer runs on battery; it runs at the lowest
    processor priority, below a requested build, with the normal disk
    priority (`OSCHESS_BRIDGE_BACKGROUND_MODE=background` runs it in
    Windows's background mode, which lowers its disk priority too). It gives
    way to the answers about any database's games and positions (lists,
    searches, sorts, a game, suggestions and explorer answers): while one
    runs, the build waits between its batches until none runs, for at most
    half a second at a time, so that it still ends if they never stop. A
    requested build never gives way, and a background build that a request
    for its database's positions makes the requested one gives way no
    longer.

  A build reads header and move records, a few megabytes at a time, never
  annotations, and needs no temporary space. It needs free space of about 400
  bytes a record, and 256 MiB beside them (about 5 GB for the Mega Database),
  on the disk of the index folder, counting the database's former index
  files, which it deletes first. With less, a request is answered
  `503 index_unavailable` saying so, and the build of a database in use waits
  for the database's next change. The notable games rendered for answers are
  kept for all databases together, within the budget (a 64th of it, at most
  8 MiB), and searches that need the memory drop them. Until the index is
  ready, requests are answered `409 database_unavailable` with
  `state: "indexing"` and `progress: {"phase", "done", "total"}`, and only
  while a build runs or waits to run; the phases are `waiting` (queued behind
  another build), `checking` (records), `reading` (records), `positions` (the
  tree's entries) and `structures` (the deep section's postings), and a
  client shows any other as it shows these (see "Compatibility").
  `/v1/status` lists the builds under `indexing`. A build that fails is
  answered `503 index_unavailable` for a minute, and the next request tries
  again; a failed build of a database in use is not tried again unasked until
  the database changes, unless it failed only because searches kept every
  search worker taken for as long as it waited for one: such a build is tried
  again unasked within a minute.
- **Changes.** The index belongs to the database's generation, and a change to
  the database rebuilds its index, about half a minute for the Mega Database
  on a quiet machine: at the next request for its positions, or, for a
  database in use, once it has not changed for a minute (see **Building**).
  Until the new index is ready the answer is `409` with `state: "indexing"`:
  an index is never answered for another generation than its own. Games
  appended to a database are a change like any other. Updating an index from
  the games appended to a database is a possible later optimisation, only if
  its results can be shown equal to a build from nothing.
- **Storage.** Index files live in the bridge's index folder, two per
  database: the index (`<id>.idx`) and its move stream (`<id>.moves`),
  built together. On Windows the folder is `%LOCALAPPDATA%\oschess
  bridge\index`, apart from the data folder, since `%APPDATA%` roams with
  the user's profile (#147); elsewhere, and wherever `OSCHESS_BRIDGE_HOME`
  sets the data folder, it is the data folder's `index`. The heads and names
  files of the databases' searches live there too; a PGN file's header index
  lives in the data folder's `pgn` ([PGN files](#pgn-files)). A bridge that
  keeps its indexes apart deletes, when it starts, the files it kept in the
  data folder's `index` before, which a new version of the index would
  rebuild anyway, and the folder once nothing else is in it.
  `docs/format-notes.md`, "Position index" and "Move stream", describes the
  files. A file that is damaged or of another version, or an index and a
  stream of different builds, are rebuilt. A build needs no room but its
  files': it writes them as `<id>.moves.partial` and `<id>.idx.partial`,
  renamed at its end, and deletes the database's former files when it
  starts. The move stream takes 68 bytes a game and 2 bytes for each ply
  past the 21st, each game with a CRC checked whenever it is replayed; a list
  of a position's games reads every game's first plies, 4,096 games at a
  time with a CRC checked the first time a list reads them, and what it
  finds there must be as many games as the index counts, else both files are
  built again. For the Mega Database, the stream is some 2 GB, and its
  index about as much, 0.85 GB of it the deep section. The stream is mapped
  read-only: the operating system keeps as much of it in memory as it can
  spare, outside the search memory.
  On Windows a build replaces a file still mapped by an answer in flight
  once that answer is done. When the bridge starts, after each change of the
  database list, and at least once a minute while the list is asked for, the
  folder is swept (#60):
  - a build's leftovers go at once unless that build is running;
  - the index and move stream of a database that has been off the list for
    ten minutes go.
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

### `GET /v1/engine/warm`

Starts the engine before an analysis asks for it (#110). Stockfish's own start
(the process, its network and its hash) takes about half a second, and it
recurs at the first analysis after ten minutes without one. The web app asks
for a warm-up when an analysis page opens with the bridge's engine chosen, and
when that page is shown again after a while.

| Parameter | Meaning |
|---|---|
| `threads` | As for `/v1/engine/analyze`: the `Threads` the next analysis will ask for. |
| `hash` | As for `/v1/engine/analyze`: its `Hash` in MB. |

The engine's process starts when none runs. `Threads` and `Hash` are sent where
they differ from the process's, then `isready`, and the answer comes once the
engine is ready:

```json
{"engine":"ready"}
```

An analysis with the same `threads` and `hash` then sends neither and only
searches. While an analysis runs, the engine is left to it and the answer is
`{"engine":"busy"}`. A warm-up counts as use: the ten minutes start again from
it. Bad `threads` or `hash` are `400 bad_request`, as for `analyze`; without
an engine the answer is `409 no_engine`, and an engine that cannot be started
or is not ready is `502 engine_failed`. A client ignores the answer: an older
bridge without this route answers `404`.

## Pairing

On first run, and from the tray menu's «Open oschess», the bridge opens

```
https://oschess.org/library?source=chessbase#cb-bridge=<token>&port=<port>
```

in the default browser. The fragment never reaches a server. The web app reads
it, stores the token and port for this browser, and removes the fragment from
the address bar before it renders. The tray menu's «Pairing code…» shows the
same token in the settings, for pasting by hand.

## Fake bridge

The oschess end-to-end tests run a fake bridge that implements this document
with synthetic games and no database. It must answer the access checks,
errors and states above exactly as described here, so that the web app's
handling of each is tested. Changes to this document therefore land in the
bridge first and in the fake bridge next.
