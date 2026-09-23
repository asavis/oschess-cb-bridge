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
| 400 | `query_syntax` | `q` cannot be parsed; `offset` is the character offset of the problem |
| 400 | `unsupported_qualifier` | `q` uses a qualifier ChessBase databases do not have; `qualifier` names it |
| 401 | `unauthorized` | Token missing or wrong |
| 403 | `forbidden_origin` | `Origin` not on the allowlist |
| 404 | `not_found` | No such path, database or game number |
| 405 | `method_not_allowed` | Not `GET` or `OPTIONS` |
| 409 | `database_unavailable` | The database is not `ready`; `state` gives its state |
| 413 | `body_not_allowed` | The request has a body |
| 421 | `misdirected_host` | `Host` is not a loopback name |
| 422 | `not_a_game` | The record is a guiding text or an analysis, which the bridge does not serve as PGN |
| 422 | `unreadable_game` | The game's records are damaged and stay so between reads; `reason` says how, in English |
| 431 | `headers_too_large` | Request line and headers over 16 KiB |
| 500 | `internal` | A bug; the bridge logs it |
| 503 | `database_changing` | ChessBase changed the database during the read; `Retry-After: 1` |
| 503 | `busy` | Too many open connections; `Retry-After: 1` |

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

## Endpoints

### `GET /v1/status`

```json
{
  "bridge": { "version": "0.4.0", "api": 1 },
  "databases": { "ready": 9, "opening": 0, "missing": 1, "cloudOnly": 0, "unsupported": 2, "unreadable": 0 }
}
```

The web app calls it first. `api` below the version it was written for means
the bridge is too old; the app then offers the download link.

### `GET /v1/databases`

The databases of ChessBase's own database window, in its order (#20), followed
by those added in `bridge.toml`.

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
| `name` | The file name without extension, as ChessBase shows it |
| `format` | `2cbh`, `cbh` or `pgn`; another value is possible later |
| `state` | `ready`; `opening` (being opened, retry after a moment); `missing` (the file is gone); `cloudOnly` (a OneDrive file not on this computer: make it available offline); `unsupported` (a format the bridge does not serve); `unreadable` (the files are present but cannot be opened: damaged, or locked by another program) |
| `records` | Games, guiding texts and analyses; present when `ready` |
| `generation` | See above; present when `ready` |

### `GET /v1/databases/{id}/games`

One window of the database's records, sorted and optionally searched.

| Parameter | Default | Meaning |
|---|---|---|
| `offset` | `0` | The first row of the window, counted from 0 in the sorted order |
| `limit` | `200` | Rows in the window, 1 to 500 |
| `sort` | `number` | `<key>`, `<key>-asc` or `<key>-desc`; keys below |
| `q` | | A search in the Library search grammar (#21, `docs/search-grammar.md`) |

Sort keys: `number`, `white`, `black`, `whiteElo`, `blackElo`, `result`,
`moves`, `eco`, `tournament` (alias `event`), `date`, `round`, `annotator` —
the oschess Library's keys plus `number` and the two Elo keys. Without a
direction, `date` and `moves` sort descending and the others ascending, as in
the Library. Ties are broken by `number`, ascending.

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
| `lang` | `en` | Comma-separated language preference for annotation text, for example `uk,en` (#22) |

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
| `pgn` | The game, with its variations and, from #22, its annotations: comments in `{}`, symbols as NAGs, coloured squares and arrows as `[%csl ...]` and `[%cal ...]` |
| `annotations` | `none`, `complete`, or `incomplete` when an annotation of unknown layout stopped decoding; the PGN then has the annotations before it |
| `unreadableAnnotation` | With `incomplete`: the annotation type code, a number |

A guiding text or an analysis is answered `422 not_a_game`, and a game whose
records are damaged `422 unreadable_game`. Deleted games are served.

### `GET /v1/databases/{id}/suggest` (planned, #21)

| Parameter | Meaning |
|---|---|
| `field` | `player`, `event` or `annotator` |
| `prefix` | The typed beginning, case-insensitive, at least 1 character |
| `limit` | At most 20, the default |

```json
{ "field": "player", "suggestions": [ { "value": "Morphy, Paul", "games": 211 } ] }
```

Most games first, then alphabetical.

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
