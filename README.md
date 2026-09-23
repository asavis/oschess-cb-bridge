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
same options, for development. On Windows, `oschess-bridge` also opens the
pairing link in the browser on its first run, and a second start asks the
running bridge to open oschess instead of starting another. `web` in
`bridge.toml` points the pairing link at another allowed site, such as
`https://staging.oschess.org`.

A database path may name the `.2cbh` file or the common stem of its files.
Databases are opened read-only and read with positional reads; nothing is
written, and nothing leaves the machine except what the bridge serves to the
oschess page on this computer.

## Install on Windows

1. Download `oschess-bridge.exe` and `oschess-bridge.exe.sha256` from the
   [Releases](https://github.com/asavis/oschess-cb-bridge/releases) page.
2. Check the download. In PowerShell, in the download folder, run
   `(Get-FileHash .\oschess-bridge.exe -Algorithm SHA256).Hash.ToLower()`.
   The result must equal the first word of `oschess-bridge.exe.sha256`, which
   the release notes also show. If it differs, delete the file.
3. Start `oschess-bridge.exe`. It is not signed yet, so Windows SmartScreen
   warns that it protected your PC. Choose **More info**, then **Run anyway**.
4. On the first start the bridge opens oschess in the browser with the pairing
   link, which connects the Library's «ChessBase» section to it. For now the
   bridge runs in a console window; closing the window stops it.
5. The browser asks whether oschess may access devices on your local network.
   The bridge listens on this computer only, and the Library needs that
   permission to reach it: allow it. If it was blocked, allow local network
   access for oschess in the site settings (the icon to the left of the
   address) and reload the page.

The bridge works with Chrome and Edge 142 or later on Windows. Firefox has not
been verified yet.

## Contributing

See [CLAUDE.md](CLAUDE.md) for the repository rules, including the
cross-model review every change goes through.

## Legal

MIT licensed; see [LICENSE](LICENSE). ChessBase is a trademark of ChessBase
GmbH. This project is not affiliated with, endorsed by or sponsored by
ChessBase GmbH, and contains no ChessBase code or data.
