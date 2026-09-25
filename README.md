# oschess-cb-bridge

Read ChessBase chess databases with open-source code, on the machine that
holds them: the ChessBase 2 format (`.2cbh`, `.2cbg`, `.2lid`, …, written by
ChessBase 17 and later) and the classic format (`.cbh`, `.cbg`, `.cba`, …).
PGN files (`.pgn`) are read as databases too.

The repository is a Cargo workspace:

| Crate | What it is |
|---|---|
| [`cbformat`](crates/cbformat) | A reader library for both formats: game headers, players and tournaments, the full move tree of every game, checked move by move on a board, and its annotations. PGN output. A PGN file read as a database through a header index (`pgnfile`). `view::Base` reads every format the same way. |
| [`cbtool`](crates/cbtool) | A command-line tool over the library: `info`, `verify`, `pgn`, `databases`, and `bridge` to run the bridge in a console. |
| [`chesscore`](crates/chesscore) | A dependency-free chess core: positions, legal moves, FEN, Chess960 and the Polyglot position key, tested against published perft counts and a `cozy-chess` oracle. |
| [`bridge`](crates/bridge) | `oschess-bridge`: serves the databases of this machine to the [oschess](https://oschess.org) web app over a loopback-only HTTP API, specified in [docs/api.md](docs/api.md). |
| [`app`](crates/app) | The bridge for Windows as a tray app built with [Tauri 2](https://tauri.app): the bridge's state in the tray, a status flyout, settings and a first-run window. Released as `oschess-bridge.exe` and its installer. Windows only; elsewhere it builds as a stub. |

## Install

The bridge runs on Windows 10 and 11. Your databases stay on your computer:
the bridge serves them only to the oschess page in a browser on the same
computer.

1. Open the [latest release](https://github.com/asavis/oschess-cb-bridge/releases/latest)
   and download `oschess-bridge-setup.exe`. Your browser may warn that the file
   is not commonly downloaded; keep it. The release notes give its SHA-256,
   which `Get-FileHash .\oschess-bridge-setup.exe` in PowerShell prints too.
2. Run it. While releases are unsigned, Windows SmartScreen says «Windows
   protected your PC»: choose **More info**, then **Run anyway**. Where Smart
   App Control is on (Windows 11, Windows Security → App & browser control),
   Windows blocks unsigned programs outright and offers no way past it; such
   a computer needs a signed release. The release notes say whether a release
   is signed.
3. The installer needs no administrator: it installs for your Windows user
   into `%LOCALAPPDATA%\oschess bridge`, in Ukrainian or English after the
   Windows display language. Choose **Next** through its pages and
   **Install**; if Windows lacks Microsoft's WebView2 runtime, the installer
   downloads it first. On the last page leave «Run oschess bridge» ticked and
   choose **Finish**.
4. The bridge starts in the tray, as the oschess mark by the clock. The «The
   bridge is installed» window opens, and so does your default browser, on the
   oschess Library with the pairing link.
5. The browser asks whether oschess may access devices on your local network.
   Choose **Allow**: the bridge is on your own computer, and this lets the page
   reach it. The Library's ChessBase section connects, and the tray mark's
   tooltip says how many databases are ready.

If you chose **Block**, or take the permission back later, the ChessBase
section says so. Allow it again in the site's settings (the icon to the left of
the address → Site settings → Local network access), and the page connects
without pairing again. If the page did not open or did not connect, the
«The bridge is installed» window shows the pairing code: in oschess, open the
Library → ChessBase → «Paste the code by hand».

Browsers: Chrome and Microsoft Edge 142 or later. Firefox has not been tried
yet.

Right-click the tray mark for the menu: open oschess (it pairs the browser it
opens, so another browser or profile connects with nothing to copy), the
settings, the pairing code, «Start with Windows» (off until you tick it), checking for updates, and
quitting. Updates install by themselves while the bridge is idle, unless
«Update automatically» is off in the settings. Idle means that no database is
downloading or opening, no position index is being built, no Stockfish is
being installed, and no analysis began less than five minutes ago; an
analysis left running longer does not hold an update back. A version whose settings say
«This build does not check for updates» is replaced by running the next
installer by hand. To remove the bridge, use
Settings → Apps → Installed apps → oschess bridge → Uninstall. Its data folder,
`%APPDATA%\oschess-bridge`, stays: the settings (`bridge.toml`), the pairing
code, the position indexes (`index`, some 1.4 GB for the Mega Database) and any
Stockfish the bridge installed (`engines`). Delete the folder by hand to remove
them too.

The release also has `oschess-bridge.exe`, the same app without the installer.
Updates come as the installer, so they install the bridge for your user as
above.

## Status

The reader decodes every game of a Mega Database 2026 — 11,964,285 games and
963 million plies, including Chess960 games, set-up positions, variations and
null moves — with every move checked for legality and for the piece it
captures, in about 20 seconds on a desktop. Annotations are decoded for all
423,390 annotated games of that database. Comments, symbols, coloured squares
and arrows go into the PGN for reading; the full PGN form
(`annotations=full`, [docs/api.md](docs/api.md)) also keeps every other
annotation type, such as training questions, clocks and evaluations.

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
target/release/cbtool profile "path/to/Database.2cbh" --index "path/to/scratch"   # timings
```

`cbtool info`, `verify` and `pgn` also take a classic database
(`Database.cbh`), and the bridge serves one like a 2CBH database
([docs/api.md](docs/api.md#classic-databases)). `--lang` chooses the language
of comments stored in several languages, in order of preference; English is
the default.

`cbtool info` and `verify` also read a PGN file as the bridge does: they build
its header index and play every game's main line as written, printing counts.
`--code-page N` reads text that is not UTF-8 in Windows code page N; the
bridge uses the computer's. The bridge serves a PGN file like a 2CBH database
once it has read it ([docs/api.md](docs/api.md#pgn-files)). `cargo run
--release -p cbformat --example pgn_pairs -- <db.2cbh> <export.pgn>` compares a
database with its PGN export field by field and move by move, and `cargo run
--release -p bridge --example pgn_index_pairs -- <db.2cbh> <export.pgn> <dir>`
compares their position indexes; both print counts only.

`cbtool profile` times the bridge's flows against one database (#83). It
serves the database from a bridge in its own process, on a free port, and asks
it over HTTP as oschess does:

- the first answers after the start;
- the first sort by each key, then cached;
- windows of 500 rows at the start, middle and end of the list, with and
  without `line=60`;
- player suggestions for one to three letters, then searches by player,
  colour, event and date, cold and cached;
- one game as PGN, the first one and the most annotated of the first 50,000,
  in both forms;
- the position index's build in the `--index` folder, a lookup per move and
  opening it again;
- a 5-second search when `--engine <exe>` names a UCI engine;
- a small answer over a kept and over a new connection.

It prints only timings and counts, never a name, game, query or path, so its
output can go into an issue as it is. Run it on the computer the bridge serves,
against the database's own folder: on Windows from a WSL mount the file
reads, not the bridge, set the times.

`oschess-bridge` listens on `127.0.0.1:39581` and keeps `bridge.toml` and its
pairing token in its data folder (`%APPDATA%\oschess-bridge` on Windows).
`--show-token` prints the pairing link that connects oschess to it, and
`--new-token` replaces the token. `cbtool bridge` runs the same bridge with the
same options, for development. `web` in `bridge.toml` points the pairing link
at another allowed site, such as `https://staging.oschess.org`.

Searches use the oschess Library's grammar
([docs/search-grammar.md](docs/search-grammar.md)) and read the headers on
workers that all searches share, one per core and at most 16;
`OSCHESS_BRIDGE_THREADS` sets another number. Their memory stays within 1 GiB,
or `OSCHESS_BRIDGE_SEARCH_MIB` ([docs/api.md](docs/api.md#search-memory)).

The first request for a database's positions builds its position index in the
background, on half of those workers and within the same memory, and keeps it
in the data folder's `index` folder: the games, results, moves and notable
games of every position the games reach in their first 40 plies. For the Mega
Database it takes some five minutes and 1.4 GB. Once kept, the index answers
from the first request after the bridge starts, until the database changes.
`cargo run --release -p bridge
--example index_oracle -- <db.2cbh> <index dir>` builds one and checks it
against a brute-force count, printing numbers only. `cargo run --release -p
bridge --example classic_pairs -- <scratch dir> <a.cbh> <a.2cbh> …` serves the
classic and the 2CBH copy of each database given and compares their rows,
searches, sorts, suggestions and explorer answers field by field, printing
counts only; it fails on any difference the classic format does not explain.

It serves the databases ChessBase's database window shows, read from
`DBItems.cbini` in `Documents\ChessBase`. The Documents folder is the one
Windows reports, wherever it has been moved; `OSCHESS_BRIDGE_DOCUMENTS` names
another one, and outside Windows only that variable gives one. Databases or
folders of databases listed under `databases` in `bridge.toml`, and
`--database`, add more. The list is read again when those files change. A
database kept only in the cloud is not read while it is listed; opening it in
oschess downloads it first ([docs/api.md](docs/api.md#cloud-only-databases)).

A database path may name the `.2cbh` or `.cbh` file, or the common stem of its
files, which is read as 2CBH when a `.2cbh` file has it.
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
folders, the port, the pairing code, starting with Windows, updates), checks
for updates or quits. The
windows are plain HTML, CSS and JavaScript in `crates/app/ui`, in Ukrainian or
English after the Windows display language; `crates/app/icons/generate.py`
draws the tray marks and the app icon from the logo.

The app builds on Windows only. On Linux, `scripts/clippy-windows.sh` checks
it for `x86_64-pc-windows-gnu` (`rustup target add x86_64-pc-windows-gnu`)
without linking.

A version tag builds the release: `oschess-bridge.exe`, its per-user NSIS
installer and their SHA-256, as a draft for the owner to publish
([docs/release.md](docs/release.md)). The app's updater reads the newest
release's `latest.json` and installs only an installer signed with the updater
key, whose public half is in `crates/app/tauri.conf.json`.

## Contributing

See [CLAUDE.md](CLAUDE.md) for the repository rules, including the
cross-model review every change goes through.

## Code signing policy

Free code signing provided by [SignPath.io](https://about.signpath.io),
certificate by [SignPath Foundation](https://signpath.org). Until SignPath
Foundation accepts this project, releases are unsigned; the release notes say
whether a release is signed.

What is signed: only this repository's own `oschess-bridge.exe` and its
installer, as `.github/workflows/release.yml` builds them on GitHub's runners
from a version tag on `main`. Every release is published by the owner after
checking it ([docs/release.md](docs/release.md)).

Roles:

- Committers and reviewers: [@asavis](https://github.com/asavis), the owner,
  and the owner's two automated contributors,
  [@oschess-claude-bot](https://github.com/oschess-claude-bot) (Claude) and
  [@oschess-codex-bot](https://github.com/oschess-codex-bot) (Codex). A
  change written by one of the two is reviewed by the other before it is
  merged ([docs/review.md](docs/review.md)).
- Approvers: [@asavis](https://github.com/asavis), who approves every signing
  request.

Privacy: the bridge sends no information about you, your computer or your
databases anywhere. It opens your databases read-only and serves them only to
the oschess page in a browser on the same computer, over the loopback address
`127.0.0.1`, and only to a browser that holds the pairing code. Beyond this
computer it connects only to GitHub, to look for a new release of the bridge
and download it; that request carries nothing about you or your databases, and
«Update automatically» in the settings turns the automatic looks off. The
installer downloads Microsoft's WebView2 runtime from Microsoft when Windows
lacks it, and opening a database kept only in the cloud makes Windows download
its files from your cloud storage, as opening them in any program does.

## Legal

MIT licensed; see [LICENSE](LICENSE). The oschess name and logo, in
`crates/app/icons` and `crates/app/ui/img`, are not covered by the MIT licence;
see [crates/app/icons/NOTICE](crates/app/icons/NOTICE). ChessBase is a trademark of ChessBase
GmbH. This project is not affiliated with, endorsed by or sponsored by
ChessBase GmbH, and contains no ChessBase code or data.
