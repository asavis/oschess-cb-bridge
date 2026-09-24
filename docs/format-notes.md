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

## Annotations (`.2cba`)

Annotations carry no length field, so a reader must know the layout of every
type it meets. The census below (`examples/annotation_census.rs`) decodes every
record of the Mega (423,390 games with annotations), of 46 weekly Mega updates
(278,306 records) and of the 306 smaller databases (64,207 annotated games) with
no damaged record and no type left undecoded. What it needed beyond the
description:

**Squares and arrows** (`04`, `05`): the `int` length is followed by pairs
(colour, square) and triples (colour, from, to), as in the classic format.
Squares are **numbered from 1, file by file**: `a1` 1, `a2` 2, `b1` 9, `h8` 64.
Evidence: for arrows on games without variations, the arrow's origin holds a
piece after the annotated move in 287 of 343 cases when read file by file, and
in 129 when read rank by rank. Colours are 2 green, 3 yellow and 4 red; 7, 8 and
9 also occur (155 + 117 + 26 squares, 785 + 689 + 337 arrows in the Mega) and
are **unknown**.

**Symbols** (`03`): the three bytes are NAG numbers: `!`-type symbols 1-8, 22
and 32 on the move; 8, 10-19, 32-44, 130-138 and 146 (novelty) on the position;
140-145 as the prefix.

**Game quotation** (`13`):

- The two rating-list names after the `01 00 01 00 00` markers are an `int`
  length and that many bytes, not the length-byte strings of the header.
- Of the 29 bytes before the move count, byte 26 is 1 for a quoted game from
  the standard position. It is 0, once in the Mega, for a set-up start: 64 bytes
  follow, one per square file by file (1 king, 2 queen, 3 knight, 4 bishop,
  5 rook, 6 pawn, +8 for black, 0 empty), then 11 bytes that are **unknown**,
  then the last two of the 29.

**Training** (`09`): the third byte of the header is the variant.

| Variant | Solutions (after the `byte` count) |
|---|---|
| 1 | two squares, two unknown bytes, a list |
| 2 | a points byte, then two lists: the answer and the reply shown for it |

Variant 2 occurs in 29 Mega games, with multiple-choice questions.

**Web link** (`1c`): `01`, then the URL and the caption, each an `int` length
and that many bytes. 26 in the Mega.

**Video** (`20`): `01 00`, a `short` that is likely a language, an `int` length
and that many bytes. 22 in the Mega.

**Type `27`**: not in the description. Two bytes, always on a move: the short
1 in all 25 samples of the updates; 28 occur in the Mega. Its meaning is
**unknown**.

**Positions** follow the description: PGN order, with each alternative right
after the move it replaces. No annotation in any 2CBH database examined names a
position past the last move. The reader writes one after the main line's last
move, as for the classic format (below); only in a game without moves is it
damage. Two symbol annotations in the Mega sit on position −1, the game as a
whole; PGN has no place for a NAG before the first move, so they are not
written.

**Text languages** in the Mega: 798,578 English, 783,156 "any language" (7),
320,588 German, then French, Spanish, Portuguese, Dutch, Italian, Polish and
Greek below 4,000 each.

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

**The order.** The file keeps the entries in its own order: the `2cbg` section
first, then `Databases`. The window sorts by `Sort` and `SortDir0`–`SortDir7`.
- **`Sort` 6** (all `SortDir` 0) is the only setting seen, and ChessBase's default.
  With it, the window orders by **a**, the icon number, largest first, and
  entries with the same icon by title, last first, without regard to case.
- **The evidence:** the owner's window, in its icon view and in its detail view
  (whose columns show no sort mark), matched this order for all ten databases,
  three of which share icon 0 (asavis/oschess-cb-bridge#20).
- **Other settings are not decoded:** another `Sort`, any `SortDir` other than 0,
  or a missing one. For them, `DbList::window_order` keeps the file order.

**Items the window adds itself.** The window also lists ChessBase's clip database
(`NoGames\ClipDBs\CBMain.cli`, format "CLI") and the ChessBase Cloud clip
(`Cloud\<account>\cloudclip.cbcloud`, format "Cloud"). Neither is in
`DBItems.cbini`, and neither is a database file the bridge reads, so the bridge
does not list them.

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

## Position index (the bridge's own files)

Not a ChessBase format: the files the bridge writes for
`GET /v1/databases/{id}/explorer` (`docs/api.md`), one per database in the
data folder's `index` folder, `<id>.idx`. Integers are little-endian.

- **Key.** A position is its Polyglot key (`chesscore::Board::hash`), which
  adds an en passant square only when a capture is possible. Chess960 games
  are left out, since the key names a castling right by its side, not by its
  rook.
- **What a game adds.** Each position of its main line from the start to
  ply 40, once however often it is reached, with the move played from it up to
  ply 40; a position reached again adds only the move from its first visit.
  A null move or damaged moves end the line there; the position before them
  counts, and no move from it. A game whose move record is over 2 MiB, or
  cannot be read, adds nothing. A position reached by one game only beyond
  ply 20 is dropped.
- **Header** (128 bytes):

  | Offset | Size | Field |
  |---|---|---|
  | 0 | 8 | magic `OSCBIDX\0` |
  | 8 | 4 | format version, 1 |
  | 12 | 4 | header length, 128 |
  | 17 | 1 | depth in plies, 40 |
  | 18 | 1 | pruning ply, 20 |
  | 20 | 4 | first record indexed |
  | 24 | 4 | last record indexed |
  | 32 | 8 | the database's generation when built |
  | 48 | 8 | games indexed |
  | 56 | 8 | positions |
  | 64 | 4 | blocks |
  | 72 | 8 | offset of the block table |
  | 80 | 4 | CRC-32 of the block table |
  | 88 | 8 | file length |
  | 124 | 4 | CRC-32 of bytes 0-123 |

  The other bytes are zero.

- **Blocks** follow the header back to back, positions in ascending key order,
  up to 4,096 a block, and a block ends once its records reach 1 MiB. A block holds its keys, 12 bytes each (the key, then the
  record's offset in the block's data, 4 bytes), then the records.
- **A record** is unsigned LEB128 numbers:
  - the games, white wins, draws and black wins;
  - the number of moves, then each move as 2 bytes (from square, to square,
    and for a pawn reaching the last rank the promotion piece, queen, rook,
    bishop or knight as 0-3, in bits 0-5, 6-11 and 12-13; castling is the king
    moving onto its rook) followed by its four counts;
  - the number of notable games, then their numbers, the highest average
    rating first, then the latest.
- **The block table** ends the file: for each block its first key, offset (8
  bytes each), number of keys, data length and the CRC-32 of its keys and data
  together (4 bytes each), 28 bytes a block.
- **Checks.** On opening, before anything is allocated from the header's
  counts: the header's CRC, and counts that fit the file (the table between
  its offset and the end of the file; 1 to 4,096 keys a block; for every key
  12 bytes and a record of at least 6 before the table). Then the table's CRC,
  and blocks that follow each other, in key order, with at most 4,096 keys and
  a little over 1 MiB of records each. On each lookup, the CRC of the block
  read. Any failure rebuilds the index. The table is held within the search
  memory budget while the index is open.
- **Deciding what to build.** An index built at the database's generation is
  current; any other is rebuilt.
- **The build.** Workers read the header records and the move records of 2,048
  games at a time into buffers reserved in the search budget, about 3 MiB a
  worker: the run's move records at once when they fit a 2 MiB window of
  `.2cbg`, else one record of at most 2 MiB at a time; never `.2cba`. They
  turn their share of the games into 16-byte entries:
  the key, the game number (30 bits) with its result (2), and the move (14),
  ply (6) and average rating (12). Each worker sorts its entries within its
  share of the search memory budget and writes them as runs. The runs are then
  merged, each run with a 64 KiB read buffer: in passes of as many runs as half
  the budget holds, at most 256, until the final merge can take all that are
  left beside the writer's 6 MiB. That final merge adds up each position's
  entries into its record as it passes them. Half the budget is at least
  8 MiB, which holds the writer and 30 runs; a smaller share fails the build
  as too large rather than waiting for memory the build holds itself.

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
  lies on the main line, and where each variation opens and closes) is
  identical to the 2CBH copy's, and so are the start position, result, ECO,
  date, ratings, round, players and tournament, apart from the names listed
  below. The PGN of every game, annotations included, is byte for byte the
  2CBH copy's, with English and with German comments; the only exceptions are
  87 games of the user database whose tags carry one of those names.
  `cbtool verify` prints the same counts for both copies of each pair,
  annotations included, and the rules oracle (`examples/oracle.rs`, which
  replays the decoded moves in `cozy-chess`) finds no difference in their
  2,617,380 positions;
- the first 4,971 records of a Mega Database 2026 converted to `.cbh` by
  ChessBase and stopped part way: its 4,964 game trees, among them 422 set-up
  positions and 157 null moves, equal the 2CBH Mega's first 4,964;
- 245 databases that exist only in `.cbh` (40,635 games, 31,196 of them from
  set-up positions, 1,628 guiding texts, 6,854 null moves, 30,485 annotated
  games): 244 verify with no failure. One is damaged (below), and one holds
  two games with annotations past their last move (below), which `verify`
  counts.

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
  them: their home squares, or in Chess960 the squares the record names
  (`cbh::start_as_played`); otherwise the move is an error. The stored
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
- **Name fields keep their terminating zero.** In the 251 classic databases
  examined no last name fills its 30 bytes and no first name its 20: the
  longest hold 29 and 19 bytes. Tournament titles hold at most 39 of their
  40 bytes (5,355 do), except in the user database ChessBase converted from
  2CBH, where 15 fill all 40. A cut is measured in the bytes the field
  stores, not in the text read from them: a Windows-1252 byte reads as up to
  three bytes of UTF-8. A name cut in UTF-8 ends before a character that
  would cross the end: a first name of ten two-byte characters keeps nine.
- **2CBH analyses** are stored as ordinary games in a converted classic
  database (5 in the partial Mega); their move trees are the same.
- **Guiding texts** keep their titles, one per language, at the head of their
  own `.cbg` record, where 2CBH names a title as a game tag entity. Of the
  1,628 texts in the classic-only databases, none names a tournament in its
  header; 297 name an annotator and 339 a source. The bridge shows a text's
  first title that is not blank, whatever its language, and its annotator as
  its author (`cbh::Database::text_title`).
- **Annotators** are a table of their own (`.cbc`), one text each, where 2CBH
  names a player and shows it as `Last, First`. In the user database, 290 of
  the 291 games the 2CBH copy annotates carry the same words in another order
  in the classic copy (`First Last`), and one is equal; two Mega updates have
  2 such games each. The bridge searches, sorts and suggests the classic copy
  by its own annotator text, as stored.
- **Damage.** One old database fails on 3 of its 385 games: a game whose moves
  stop without an end marker, a game whose record head gives size 0 (it starts
  where the previous game's record ends), and a deleted game whose record
  claims mode 62 and 8 MB past the end of the file. The reader reports each as
  an error.
- **Large files.** Offsets in `.cbh` are 32-bit. When `.cbg` or `.cba` is
  larger than 4 GiB, the reader takes each game's offsets from its `.cbj`
  record instead, where they are 64-bit: moves at 0x1e and annotations at
  0x0c, big-endian. `.cbj` is read only then. Its record must be long enough
  to hold both (38 bytes, version 6 on), and the low 32 bits of each must
  equal the `.cbh` offset, or the game is an error. In the six paired
  databases, every game's `.cbj` offsets equal its `.cbh` offsets.

## Annotations (`.cba`)

A game's `.cba` record is a 14-byte head (the game id, the bytes `01 00 0e 0e`,
the number of annotations plus one, and the record's size) followed by the
annotations, each with its position, its type and its own size. Integers are
big-endian. The reader maps them to the same annotations as 2CBH's, so the PGN
writer handles both formats alike.

- **Positions count in stored order**: depth first, the main line first at
  every position, which is the order the moves are stored in `.cbg`. 2CBH
  counts in PGN order. The paired databases confirm it: with each format's
  numbering, every annotated game's PGN is the same.
- **Every type has its size**, so a type whose layout is unknown is skipped,
  and a classic record is never left incomplete. The types met in the paired
  Mega updates are `02` text, `03` symbols, `13` game quotation, `18`
  critical position, `22` medals and `26` evaluations; the user database adds
  `04` coloured squares and `05` arrows.
- **A text's language** is a nation number: 42 (England) is English, 53
  (Germany) German and 0 any language, as the pairs show against the 2CBH
  languages. French, Spanish, Italian, Dutch, Portuguese, Polish and Greek map
  by their nations (49, 43, 70, 103, 117, 116, 55). Any other nation is a
  language of its own, which no preference names.
- **Squares** in coloured squares and arrows are numbered from 1.
- **The file header** is 26 bytes, or 10 in old databases, as for `.cbg`: 139 of
  the 245 classic-only databases have the short header, and their first
  record starts at 10.
- **Checks.** The head's game id equals the game, and its count and size
  equal the record's contents, in every record of every database examined.
  The reader treats a mismatch, an annotation running past the record, a
  position below −1 and a square out of range as damage.
- **Past the last move.** Two set-up games of one classic-only database have
  annotations past their last move (positions 52 and 16, in games of 51 and 11
  moves), and ChessBase opens both. The reader writes such an annotation after
  the main line's last move, whatever its kind, and a text meant to precede a
  move follows it there; the game is served in full, and `cbtool verify`
  counts the games and the annotations moved. 2CBH is read the same way. In a
  game without moves there is no move to take them, so there it stays damage.
- **Record size.** A record's head may claim up to 4 GiB. The largest record
  in the 252 classic databases examined is about 45 KB, so a record over
  16 MiB (`cbh::MAX_ANNOTATION_RECORD`) is refused before it is read, as 2CBH
  records over 64 MiB are.

## Reader rules

- **Castling** is read only from its defined encodings: the one-byte codes 9
  and 10; in a Chess960 game, two bytes naming the king's destination
  (`g1` `c1` `g8` `c8` for the side to move) as both squares; in any other game,
  two bytes moving the king from `e1` or `e8` to the `g` or `c` square of the
  same rank. Any other move onto a piece of the side to move is an error.
- **Chess960 castling squares.** The six squares after a Chess960 start
  position name each side's king square and the rook each castling right
  uses. A set-up keeps them: a side keeps its rights only while its king
  stands on its named square, and a right castles with the named rook even
  when another rook stands further out, such as one promoted to. A right the
  reader adds for a castling move (above) obeys the same squares. A square
  that is not on its side's back rank names nothing: the right then needs no
  particular king square and uses the outermost rook on its wing.
- **Nesting.** At most 1,024 variations may be open at once
  (`cbh::MAX_VARIATION_DEPTH`); a deeper tree is an error, raised before its
  position is saved. The deepest nesting measured is 74 in the Mega Database
  2026 (three games reach 64 or more) and 63 in the classic databases above.
