# oschess-cb-bridge

Read chess databases in the ChessBase 2 format (`.2cbh`, `.2cbg`, `.2lid`, …,
written by ChessBase 17 and later) with open-source code, on the machine that
holds them.

The repository is a Cargo workspace:

| Crate | What it is |
|---|---|
| [`cbformat`](crates/cbformat) | A reader library: game headers, players and tournaments, the full move tree of every game, checked move by move on a board, and its annotations. PGN output. |
| [`cbtool`](crates/cbtool) | A command-line tool over the library: `info`, `verify`, `pgn`, `databases`, and `bridge` to run the bridge in a console. |
| [`chesscore`](crates/chesscore) | A dependency-free chess core: positions, legal moves, FEN, Chess960 and the Polyglot position key, tested against published perft counts and a `cozy-chess` oracle. |
| [`bridge`](crates/bridge) | `oschess-bridge`: serves the databases of this machine to the [oschess](https://oschess.org) web app over a loopback-only HTTP API, specified in [docs/api.md](docs/api.md). |
| [`app`](crates/app) | The bridge for Windows as a tray app built with [Tauri 2](https://tauri.app): the bridge's state in the tray, a status flyout, settings and a first-run window. Windows only; elsewhere it builds as a stub. |

## Status

The reader decodes every game of a Mega Database 2026 — 11,964,285 games and
963 million plies, including Chess960 games, set-up positions, variations and
null moves — with every move checked for legality and for the piece it
captures, in about 20 seconds on a desktop. Annotations are decoded for all
423,390 annotated games of that database: comments, symbols, coloured squares
and arrows go into the PGN, and every other annotation type is read by its
layout and left out.

The format is undocumented by its vendor. This reader is written from the
reverse-engineered description in the
[Morphy project](https://github.com/Yarin78/morphy/tree/main/format/v2);
[docs/format-notes.md](docs/format-notes.md) records where real databases
differ from that description.

## Use

```
cargo build --release
target/release/cbtool info   "path/to/Database.2cbh"
target/release/cbtool verify "path/to/Database.2cbh"
target/release/cbtool pgn    "path/to/Database.2cbh" --out games.pgn
target/release/cbtool pgn    "path/to/Database.2cbh" --lang de,en --out games.pgn
target/release/cbtool databases "path/to/Documents/ChessBase"   # the databases ChessBase lists
target/release/oschess-bridge --database "path/to/Database.2cbh" --show-token
target/release/cbtool bridge --database "path/to/Database.2cbh"   # the same bridge
```

`--lang` chooses the language of comments stored in several languages, in
order of preference; English is the default.

`oschess-bridge` listens on `127.0.0.1:39581` and keeps `bridge.toml` and its
pairing token in its data folder (`%APPDATA%\oschess-bridge` on Windows).
`--show-token` prints the pairing link that connects oschess to it, and
`--new-token` replaces the token. `cbtool bridge` runs the same bridge with the
same options, for development. `web` in `bridge.toml` points the pairing link
at another allowed site, such as `https://staging.oschess.org`. Searches
use the oschess Library's grammar ([docs/search-grammar.md](docs/search-grammar.md))
and read the headers on workers that all searches share, one per core and at
most 16; `OSCHESS_BRIDGE_THREADS` sets another number. Their memory stays within 1 GiB, or `OSCHESS_BRIDGE_SEARCH_MIB`
([docs/api.md](docs/api.md#search-memory)).

It serves the databases ChessBase's database window shows, read from
`DBItems.cbini` in `Documents\ChessBase`. The Documents folder is the one
Windows reports, wherever it has been moved; `OSCHESS_BRIDGE_DOCUMENTS` names
another one, and outside Windows only that variable gives one. Databases or
folders of databases listed under `databases` in `bridge.toml`, and
`--database`, add more. The list is read again when those files change. A
database kept only in the cloud is not read while it is listed; opening it in
oschess downloads it first ([docs/api.md](docs/api.md#cloud-only-databases)).

A database path may name the `.2cbh` file or the common stem of its files.
Databases are opened read-only and read with positional reads; nothing is
written, and nothing leaves the machine except what the bridge serves to the
oschess page on this computer.

## The Windows app

`crates/app` wraps the bridge in a tray app. The oschess logo in the tray takes
the colour of the bridge's state: the taskbar's own colour when the databases
are ready; amber while one opens or downloads, or when one cannot be opened or
is not found; red when the bridge cannot serve at all, such as with its port
in use. The tooltip says which. A click opens a flyout with the state and the
databases, and the right-click menu opens oschess, the settings (extra database
folders, the port, the pairing code, starting with Windows) or quits. The
windows are plain HTML, CSS and JavaScript in `crates/app/ui`, in Ukrainian or
English after the Windows display language; `crates/app/icons/generate.py`
draws the tray marks and the app icon from the logo.

The app builds on Windows only. On Linux, `scripts/clippy-windows.sh` checks
it for `x86_64-pc-windows-gnu` (`rustup target add x86_64-pc-windows-gnu`)
without linking. The installer and updates come next (#23).

## Contributing

See [CLAUDE.md](CLAUDE.md) for the repository rules, including the
cross-model review every change goes through.

## Legal

MIT licensed; see [LICENSE](LICENSE). The oschess name and logo, in
`crates/app/icons` and `crates/app/ui/img`, are not covered by the MIT licence;
see [crates/app/icons/NOTICE](crates/app/icons/NOTICE). ChessBase is a trademark of ChessBase
GmbH. This project is not affiliated with, endorsed by or sponsored by
ChessBase GmbH, and contains no ChessBase code or data.
