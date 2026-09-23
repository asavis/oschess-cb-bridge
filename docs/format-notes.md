# Format notes

The reader implements the ChessBase 2 database format (the `.2cbh` family,
ChessBase 17 and later) from the reverse-engineered description published in
the Morphy project, [`format/v2`](https://github.com/Yarin78/morphy/tree/main/format/v2).
That description carries no licence, so this repository copies none of its
text; it is a reference, and this file records only where real databases
disagree with it or add to it.

Every finding below was measured with `cbtool verify` and small probes over:

- a Mega Database 2026 of 11,966,514 records, and
- 306 smaller databases written by ChessBase '26 (282,224 games), among them
  weekly Mega updates, user databases and a database also converted to the
  previous CBH format and compared game by game through an independent CBH
  reader.

## Set-up positions: castling rights are in the second word

A start-position section that holds a set-up position begins with three words
before the pieces. The description gives them as the move number; the side to
move with the en passant file in its high byte; and the castling rights. In
real databases:

| Word | Contents |
|---|---|
| 0 | the move number |
| 1 | low byte: side to move, 0 white, 1 black. **High byte: castling rights**, 1 white O-O-O, 2 white O-O, 4 black O-O-O, 8 black O-O |
| 2 | **unknown**; zero in every set-up position examined. Read as an en passant file 1-8 when it holds one, which is unconfirmed |

Evidence, over the 2,398 set-up positions of the Mega: the high byte of word 1
takes values 0-15, is in every case a subset of the rights the placement allows
(king and rook on their home squares), and equals that full set in 2,362 of
them; word 2 is 0 in all 2,398. Games that castle from a set-up position do so
only when the high byte grants the right. Read as the description has it, 15
and 12 would be en passant files, and those games would castle without a right.

## `.2lid` header size varies

The description gives the entity-file header as 184 bytes, ending in 56 bytes of
`int` pairs. Of 317 databases, 289 have that size; the others have 216 (12),
228 (3) and 236 (3). The seven pairs at 0x80 are present in all of them, once
with `(3, 2)` in place of `(3, 1)`; what follows is either further `(−1, 1)`
pairs or zero bytes. The header size field at 0x00 is authoritative, and the
entity blocks start there.

## Promotion captures

The 16 words for a pawn capture on the rank before promotion are ordered by the
captured piece first, then by the promotion piece, both in the order queen,
knight, bishop, rook. 80,740 of the Mega's 87,099 promotion captures take a
piece of a different kind from the one promoted to, so the other order would
fail the capture check on replay.

## What `cbtool verify` checks

For every game and analysis: the record framing (magic, sizes, trailing length
and checksum), the start section, and every move word of every line, played on
a board. A word must name the piece that stands on its origin, the piece (or
nothing) on its destination, and a legal move; castling needs the right. The
Mega passes with no failure: 11,964,285 games, 963,223,086 plies, 4,472 Chess960
games, 2,398 set-up positions and 3,082 null moves.

## The database window list: `DBItems.cbini`

ChessBase keeps the databases its database window shows in `DBItems.cbini`, in
the ChessBase documents folder. Nothing public describes it. This layout was read
from two such files written by ChessBase '26 on two computers, 1.6 KB each. Both
decode to their last byte with it.

**Layout.** The file starts with the magic `0c 0b 0a 0e`, then the number of
bytes that follow as a big-endian `uint`. After that comes a flat list of
items, each laid out as:

| Size | Description |
|---|---|
| 1 | the tag |
| … | the value; its layout depends on the tag |
| 4 + n | the key: its byte length as a little-endian `int`, then the bytes |

| Tag | Value |
|---|---|
| `ff` | none. The item is a section header: the items after it, up to the next header, belong to it |
| `01` | one byte |
| `08` | a little-endian `int` |
| `19`, `1a`, `1e` | a string, laid out like the key |

Items carry no length, so a reader must know every tag. An unknown tag is an
error.

Strings are UTF-8 under tag `1e`, which ChessBase uses for text that is not
ASCII, such as Cyrillic titles. Under `19` and `1a` only ASCII has been seen.
Keys that are paths are UTF-8, Cyrillic ones included. The reader decodes
anything that is not valid UTF-8 as Latin-1, so no byte is lost. What
distinguishes `19` from `1a` is **unknown**: `1a` has held only the reference
database's path.

**Sections and items seen:**

| Section | Items |
|---|---|
| `2cbg` | one per 2CBH database, keyed by its absolute path |
| `2cbh` | `RefDB`: the path of the reference database |
| `Databases` | one per database in another format (`.cbh`, `.pgn`), keyed by its path |
| `Pathes` | none |
| `Status` | `DesktopTop` (`int`), `Selected` (the selected database's path), `Sort` (`int`), `SortDir0` to `SortDir7` (bytes) |

**A database entry** is a string whose key is the database's path and whose
value is `title,a,b,c,d,e,f`. The reader takes the six numbers from the right,
so a title may contain commas. In the files examined:

- **title** is the name the window shows. It can differ from the file name, for
  example `X (2cbh)` for `X.2cbh`.
- **a** varies (0–245). Its meaning is **unknown**; it is perhaps the icon.
- **b** is the format: 28 for 2CBH, 1 for CBH, 3 for PGN.
- **c** is the number of games, equal to the database's record count.
- **d** grows between the two files. Its meaning is **unknown**; it is perhaps a
  use count.
- **e** and **f** are ChessBase dates (`year << 9 | month << 5 | day`, as game
  dates are stored). They are consistent with the last use and the date the
  database was added.

**The order.** The reader keeps the entries in file order: the `2cbg` section
first, then `Databases`. The window sorts by `Sort` and `SortDir0`–`SortDir7`.
The reader returns these raw, but their meaning is **not decoded**, so the file
order need not be the order on screen.

**Copies named after a computer.** `DBItems-<computer>.cbini` beside the file is
a OneDrive sync-conflict copy. The same `-<computer>` suffix appears on unrelated
files in the same OneDrive folder, and no such copy exists for the computer that
wrote the current `DBItems.cbini`. ChessBase reads `DBItems.cbini`, and the
reader ignores the copies.

**Paths** are absolute Windows paths. Whether a database is a cloud-only
OneDrive placeholder is read from its Windows file attributes
(`FILE_ATTRIBUTE_RECALL_ON_DATA_ACCESS`, `_RECALL_ON_OPEN`, `_OFFLINE`), never by
opening it, so the check starts no download. A 2CBH database is checked through
every file it is read through: `.2cbh`, `.2cbg` and `.2lid`, and `.2cba` when it
is there. It is opened only when none of them is offline. Through WSL, a non-empty file with
no blocks allocated is reported as possibly cloud-only. That heuristic has not
been confirmed, because no placeholder was available.

# The classic format (`.cbh`)

`cbformat::cbh` reads the previous ChessBase format, the `.cbh` family, from
Morphy's [`format/v1`](https://github.com/Yarin78/morphy/tree/master/format/v1)
description, which has no licence either: no text or code is copied from it.
The translation tables of the move encodings are the facts the format needs;
they are taken from the description as data, and a test checks each is a
permutation.

The reader was checked against:

- six databases that exist in both formats: five weekly Mega updates of 2026
  (28,488 games) and a user database (350 games). For every game of every pair
  the whole move tree (every move of every line in stored order, whether it
  lies on the main line, and where each variation opens and closes) is identical to the 2CBH copy's, and so are
  the start position, result, ECO, date, ratings, round, players and
  tournament, apart from the names listed below. `cbtool verify` prints the
  same counts for both copies of each pair;
- the first 4,971 records of a Mega Database 2026 converted to `.cbh` by
  ChessBase and stopped part way: its 4,964 game trees, among them 422 set-up
  positions and 157 null moves, equal the 2CBH Mega's first 4,964;
- 245 databases that exist only in `.cbh` (40,635 games, 31,196 of them from
  set-up positions, 1,628 guiding texts, 6,854 null moves): 244 verify with no
  failure; the one that does not is damaged (below).

## Findings

- **Format version.** The byte at 0x05 of the `.cbh` header is 5 in the six
  paired databases, the partial Mega and one other, and 1 in the remaining
  244 examined; the description gives 1. The reader does not check it. The header's in-use size at 0x01 is 44 or 36, and the
  `.cbg` header 26 or 10 bytes, as described.
- **Encoding modes.** Every game examined uses mode 0. The reader supports the
  modes whose tables the description gives: 0, 4 and 10 (compact) and 5
  (simple); any other mode is an error naming it.
- **Stray bytes after the end.** Two games of one database carry 3 and 7 bytes
  after the final end-of-line byte, inside their records. The tree before them
  is complete, and the reader ignores them.
- **Castling without a stored right.** Four set-up games in two databases
  castle although the stored castling byte lacks that right: 0 in an older
  database, `0x0b` in another. The reader starts such a game with the right its
  castling uses added, when the king and the rook stand where the right needs
  them (`cbh::start_as_played`); otherwise the move is an error. The stored
  byte itself is still reported by `GameMoves::start`.
- **Text encoding.** The description says ISO 8859-1. Names are read as
  Windows-1252, which ChessBase, a Windows program, means by the bytes
  0x80-0x9f. A database ChessBase converted from 2CBH holds names as UTF-8
  instead: 96 player and tournament names in the partial Mega. A field whose
  bytes are valid UTF-8 is read as UTF-8; a UTF-8 text cut at the field's width
  loses its incomplete last character.
- **Names that differ from the 2CBH copy** do so for two reasons only. Names
  with characters that no single-byte code page holds are stored in some other
  form (25 in the user database). Names longer than their field are cut
  at its width (63 in the user database, 32 in the partial Mega).
- **2CBH analyses** are stored as ordinary games in a converted classic
  database (5 in the partial Mega); their move trees are the same.
- **Damage.** One old database fails on 3 of its 385 games: a game whose moves
  stop without an end marker, a game whose record head gives size 0 (it starts
  where the previous game's record ends), and a deleted game whose record
  claims mode 62 and 8 MB past the end of the file. The reader reports each as
  an error.
- **Large files.** Offsets in `.cbh` are 32-bit; a `.cbg` larger than 4 GiB,
  which needs the 64-bit offsets of `.cbj`, is refused when opened.
