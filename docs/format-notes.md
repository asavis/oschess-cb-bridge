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
