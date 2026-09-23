# oschess-cb-bridge

Read chess databases in the ChessBase 2 format (`.2cbh`, `.2cbg`, `.2lid`, …,
written by ChessBase 17 and later) with open-source code, on the machine that
holds them.

The repository is a Cargo workspace:

| Crate | What it is |
|---|---|
| [`cbformat`](crates/cbformat) | A reader library: game headers, players and tournaments, and the full move tree of every game, checked move by move on a board. PGN output. |
| [`cbtool`](crates/cbtool) | A command-line tool over the library: `info`, `verify`, `pgn`. |
| [`chesscore`](crates/chesscore) | A dependency-free chess core: positions, legal moves, FEN, Chess960 and the Polyglot position key, tested against published perft counts and a `cozy-chess` oracle. |

A local bridge service for the [oschess](https://oschess.org) analysis board —
a loopback-only HTTP API that serves an opening reference from a database on
the user's own machine — is planned and will join the workspace.

## Status

The reader decodes every game of a Mega Database 2026 — 11,964,285 games and
963 million plies, including Chess960 games, set-up positions, variations and
null moves — with every move checked for legality and for the piece it
captures, in about 20 seconds on a desktop. Annotations are not read yet.

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
```

A database path may name the `.2cbh` file or the common stem of its files.
Databases are opened read-only and memory-mapped; nothing is written and
nothing leaves the machine.

## Contributing

See [CLAUDE.md](CLAUDE.md) for the repository rules, including the
cross-model review every change goes through.

## Legal

MIT licensed; see [LICENSE](LICENSE). ChessBase is a trademark of ChessBase
GmbH. This project is not affiliated with, endorsed by or sponsored by
ChessBase GmbH, and contains no ChessBase code or data.
