# Search grammar

The `q` parameter of `GET /v1/databases/{id}/games` ([api.md](api.md)) takes
the search text of the oschess Library's search bar, so that the same bar
searches a ChessBase database. This document is the bridge's definition of that
text. The oschess web app runs the conformance corpus at the end against its
own client, so that both sides read a query the same way.

## Terms

The text is split at whitespace into terms, and a record matches when it
matches every term.

- **Words and phrases.** A bare word matches when it occurs, ignoring case, in
  either player's name, the tournament's name or the annotator's name; for a
  guiding text or an analysis, in its title or its author's name.
  `"double quotes"` make one term of several words. A `#` in front of an
  unquoted word is dropped (the Library's tag chips use it).
- **Qualifiers.** `name:value` restricts the term to one field. A qualifier
  name is ASCII letters directly followed by `:`, in any case. Its value may
  list alternatives separated by commas, and the term matches when any of them
  does: `player:morphy,tal`. A comma inside quotes is part of the value.
- **Negation.** A `-` directly before a term matches the records the term
  does not: `-result:1-0`. A `-` followed by a space is an ordinary word.
- **Unknown qualifiers** are words. `foo:bar` searches the names for the text
  `foo:bar`, and so does `Lesson3:rooks`, whose `3` stops the qualifier name.
- **Empty values** are dropped, and a qualifier left with no value is ignored.

Nothing in the text is a syntax error, just as in the Library. The bridge
therefore never answers `query_syntax`; the code is reserved for a later
grammar. The one refusal is a qualifier only the Library has (below).

### Limits

Only the first 1,024 characters are read. After 32 terms (a `sort:` token does
not count), further terms are ignored, and a value is cut to its first 256
characters after trimming.

## Qualifiers

| Qualifier | Field | Matching |
|---|---|---|
| `white:` | White's name | contains the value, ignoring case |
| `black:` | Black's name | contains the value, ignoring case |
| `player:` | either player's name | contains the value, ignoring case |
| `event:`, `tournament:` | the tournament's name | contains the value, ignoring case |
| `annotator:` | the annotator's name | contains the value, ignoring case |
| `round:` | the round as listed: `5`, or `5(2)` with a sub-round | contains the value |
| `result:` | `1-0`, `0-1`, `1/2-1/2`, `*` | equals the value; `draw`, `½`, `½-½` and `1/2` mean `1/2-1/2`, and `unknown` means `*` (any case) |
| `eco:` | the ECO code, `A00`–`E99` | starts with the value (in upper case); comparable |
| `date:` | the date as `YYYY.MM.DD`, `?` for unknown parts | see below; comparable |
| `moves:` | full moves of the main line | equals the whole number; comparable |
| `elo:` | either player's rating | equals the whole number; comparable; an unknown rating (0) never matches |
| `sort:` | the order of the results | see Sorting |

Names are shown and matched as `Last, First`.

**Guiding texts and analyses** have header layouts of their own, with a title
and an author instead of the game fields. Their title stands for the tournament
and their author for the annotator, as in their list row: words, `event:`,
`tournament:` and `annotator:` apply to them. A query with any other qualifier
term (except `sort:`) lists games only, and this holds for negated terms too.

**Library-only qualifiers.** `tag:`, `created:`, `updated:`, `is:`, `has:` and
`no:` exist in the Library but not in a ChessBase database. A query that uses
one, in any form, is answered `400 unsupported_qualifier` with the qualifier's
name in lower case. They are never searched as words.

### Comparisons

On the comparable fields a value may be `>v`, `>=v`, `<v`, `<=v` or a range
`a..b`, both ends included. A range end that is empty or `*` is open:
`2400..` means `>=2400` and `..*` is dropped. On any other field these
characters are part of the value.

- **Numbers** (`moves:`, `elo:`): a value that is not a whole number matches
  nothing.
- **ECO codes** compare as text. `>C5` and `<=C5` treat `C5` as covering every
  code that starts with it, so `eco:<=C5` includes `C59`, `eco:>C5` starts at
  `C60`, and `eco:C60..C69` includes both ends.
- **Dates** are typed as `2024`, `2024-3` or `2024-03-15`, with `-`, `.` or
  `/` between the parts, and become the stored form `2024`, `2024.03` or
  `2024.03.15`. Then:
  - a plain date matches the dates that start with it: `date:1858-12` is
    December 1858;
  - a plain value that is not a date matches the dates that contain it:
    `date:??` finds every date with an unknown part;
  - a comparison or range whose ends are dates compares the stored text, with
    the same "covering" rule as ECO codes, and skips any date with a `?` within
    the length of its longest end (`date:>=1951-07` skips `1951.??.??`);
  - a comparison or range with an end that is not a date matches nothing.

## Sorting

The results are sorted by one key. The `sort` URL parameter wins over a
`sort:` token in the text. The last `sort:` token wins; a negated one, or one
with an unknown key, is ignored. Without either, results are in number order.

A key is written `key`, `key-asc` or `key-desc`, in any case. Without a
direction, `date` and `moves` sort descending and every other key ascending.

| Key | Order |
|---|---|
| `number` | the record number |
| `white`, `black`, `annotator` | the name, ignoring case |
| `tournament` (also `event`) | the tournament's name, ignoring case |
| `whiteElo`, `blackElo` | the rating |
| `result` | `*`, `0-0`, `0-1`, `1-0`, `1/2-1/2` |
| `moves` | full moves |
| `eco` | the ECO code as shown; ChessBase's hidden sub-code does not order it |
| `date` (also `pgndate`) | year, month, day; an unknown part before any known one |
| `round` | round, then sub-round |

Unknown values (no rating, no ECO code, no date, no round, an empty or missing
name) are one key, which comes first in ascending order and last in descending
order. Names are compared in full and ignoring case: names equal but for case
are equal keys. A guiding text or an analysis sorts by its title as the
tournament (among the tournaments' names) and by its author as the annotator,
and has no other key. Records with equal keys stay in number order, ascending,
in both directions.

The Library's `name`, `title`, `created` and `updated` keys do not exist for a
ChessBase database and are ignored like any unknown key.

## Differences from the Library

- A bare word searches players, tournament and annotator; in the Library it
  searches a chapter's title and tags.
- `elo:` is new here. The oschess web app offers it for ChessBase databases
  only.
- The Library-only qualifiers are refused instead of applied.
- The sort keys are those of the list above.

## Fixture

The conformance corpus below runs against this database of ten records. The
combinations of players, dates and events are invented; the names are
historical. `-` means no value: no name (the empty entity 0), no round, no ECO
code. The ninth record is a game marked deleted, and deleted games are
searched like the others. The eighth is a guiding text: its event column is
its title and its annotator column its author, and the other columns do not
apply to it. All records share one move record; the header's
move count is what `moves:` reads.

```fixture
# number | kind    | white                 | black                 | event         | date       | round | result  | eco | moves | white elo | black elo | annotator
1        | game    | Morphy, Paul          | Anderssen, Adolf      | Paris m       | 1858.12.20 | 1     | 1-0     | C52 | 17    | 0         | 0         | -
2        | game    | Anderssen, Adolf      | Morphy, Paul          | Paris m       | 1858.12.21 | 2     | 0-1     | B20 | 32    | 0         | 0         | -
3        | game    | Morphy, Paul          | Anderssen, Adolf      | Paris m       | 1858.12.22 | 3     | 1/2-1/2 | C51 | 45    | 0         | 0         | Nimzowitsch, Aron
4        | game    | Steinitz, Wilhelm     | Lasker, Emanuel       | St Petersburg | 1895.12.13 | 1     | 0-1     | C62 | 60    | 2640      | 2690      | -
5        | game    | Lasker, Emanuel       | Capablanca, Jose Raul | St Petersburg | 1914.04.21 | 1(2)  | 1/2-1/2 | D63 | 38    | 2720      | 2725      | Nimzowitsch, Aron
6        | game    | Capablanca, Jose Raul | Lasker, Emanuel       | St Petersburg | 1914.05.10 | 7     | 1-0     | C68 | 38    | 2725      | 2720      | -
7        | game    | Tal, Mikhail          | Lasker, Emanuel       | Riga Club Ch  | 1951.??.?? | -     | *       | -   | 0     | 2300      | 0         | -
8        | text    | -                     | -                     | London        | ????.??.?? | -     | *       | -   | 0     | 0         | 0         | -
9        | deleted | Morphy, Paul          | Steinitz, Wilhelm     | London        | ????.??.?? | 5     | 1-0     | C52 | 28    | 0         | 0         | -
10       | game    | Capablanca, Jose Raul | Tal, Mikhail          | Riga Club Ch  | 1951.07.01 | 2     | 0-1     | E60 | 50    | 2500      | 2600      | Tal, Mikhail
```

## Conformance corpus

Each line is a query, `=>`, and the record numbers of the result in order;
`none` for no result, or `unsupported` and the qualifier for a refusal. Where
a line has no sort, the order is by number.

```corpus
                                   => 1 2 3 4 5 6 7 8 9 10
morphy                             => 1 2 3 9
MORPHY                             => 1 2 3 9
white:morphy                       => 1 3 9
black:morphy                       => 2
player:morphy                      => 1 2 3 9
-player:morphy                     => 4 5 6 7 10
player:morphy,tal                  => 1 2 3 7 9 10
-player:morphy,tal                 => 4 5 6
white:capa black:lasker            => 6
event:paris                        => 1 2 3
tournament:"st petersburg"         => 4 5 6
london                             => 8 9
"paris m"                          => 1 2 3
#tal                               => 7 10
"#tal"                             => none
foo:bar                            => none
Lesson3:rooks                      => none
morphy result:1-0                  => 1 9
result:1-0                         => 1 6 9
result:draw                        => 3 5
result:½                           => 3 5
result:unknown                     => 7
-result:1-0 -result:0-1            => 3 5 7
eco:C5                             => 1 3 9
eco:c5                             => 1 3 9
eco:C60..C69                       => 4 6
eco:>=D                            => 5 10
eco:>D63                           => 10
eco:<C51                           => 2
eco:..B99                          => 2
date:1858                          => 1 2 3
date:1858-12-21                    => 2
date:>=1900                        => 5 6 7 10
date:1914-04..1914-05              => 5 6
date:<1859                         => 1 2 3
date:>1951                         => none
date:>=1951-07                     => 10
date:??                            => 7 9
round:1                            => 1 4 5
round:"1(2)"                       => 5
annotator:nimzo                    => 3 5
annotator:tal                      => 10
moves:38                           => 5 6
moves:>40                          => 3 4 10
moves:30..45                       => 2 3 5 6
moves:x                            => none
elo:>=2700                         => 5 6
elo:2600                           => 10
elo:<2400                          => 7
elo:>=2700 elo:<2721               => 5 6
player:morphy sort:moves           => 3 2 9 1
player:morphy sort:moves-asc       => 1 9 2 3
tournament:"st petersburg" sort:white => 6 5 4
sort:name                          => 1 2 3 4 5 6 7 8 9 10
sort:number-desc                   => 10 9 8 7 6 5 4 3 2 1
sort:white                         => 8 2 6 10 5 1 3 9 4 7
sort:white-desc                    => 7 4 1 3 9 5 6 10 2 8
sort:black                         => 8 1 3 5 4 6 7 2 9 10
sort:black-desc                    => 10 9 2 4 6 7 5 1 3 8
sort:whiteElo                      => 1 2 3 8 9 7 10 4 5 6
sort:whiteelo-desc                 => 6 5 4 10 7 1 2 3 8 9
sort:blackElo                      => 1 2 3 7 8 9 10 4 6 5
sort:blackElo-desc                 => 5 6 4 10 1 2 3 7 8 9
sort:result                        => 7 8 2 4 10 1 6 9 3 5
sort:result-desc                   => 3 5 1 6 9 2 4 10 7 8
sort:moves                         => 4 10 3 5 6 2 9 1 7 8
sort:moves-asc                     => 7 8 1 9 2 5 6 3 10 4
sort:eco                           => 7 8 2 3 1 9 4 6 5 10
sort:eco-desc                      => 10 5 6 4 1 9 3 2 7 8
sort:tournament                    => 8 9 1 2 3 7 10 4 5 6
sort:event-desc                    => 4 5 6 7 10 1 2 3 8 9
sort:date                          => 10 7 6 5 4 3 2 1 8 9
sort:pgndate-asc                   => 8 9 1 2 3 4 5 6 7 10
sort:round                         => 7 8 1 4 5 2 10 3 9 6
sort:round-desc                    => 6 9 3 2 10 5 1 4 7 8
sort:annotator                     => 1 2 4 6 7 8 9 3 5 10
sort:annotator-desc                => 10 3 5 1 2 4 6 7 8 9
tag:x                              => unsupported tag
-is:chapter                        => unsupported is
no:tag                             => unsupported no
created:2026                       => unsupported created
UPDATED:>1                         => unsupported updated
has:eco                            => unsupported has
```

## Suggestions

`GET /v1/databases/{id}/suggest?field=player|event|annotator&prefix=…` offers
names that start with the prefix, ignoring case; for people, a first name that
starts with it counts too (`mik` offers `Tal, Mikhail`). Each name comes with
the number of games that have it in that role, either colour for `player`;
guiding texts and analyses are not counted. Names are compared in full, and
entities with exactly the same name are one suggestion: a game counts once for
it, even when both its players are entities of that name. Names without games
in the role are not offered, nor names that a quoted value cannot hold (over
256 characters, or with a double quote). The list is sorted by that count,
most first, then by name ignoring case.
