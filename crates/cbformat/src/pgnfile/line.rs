//! The main line of one game's text, played on a board: from its `FEN` tag or
//! the standard start, each move as written ([`crate::pgn::parse_san`]),
//! until the movetext ends or a move names no legal move. Variations,
//! comments and NAGs are passed over.

use chesscore::{Board, Move};

use super::lex::{Lexer, Sink, Token};
use super::scan::{ResultCode, is_both_lost};
use crate::pgn::parse_san;

/// The longest `FEN` tag read; a real one is under 90 bytes.
const MAX_FEN: usize = 128;

/// How a main line ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineEnd {
    /// The start could not be set up: the `FEN` tag names no position. Nothing
    /// was visited.
    BadStart,
    /// The movetext ended.
    End,
    /// The visitor stopped it.
    Stopped,
    /// A null move ended it.
    NullMove,
    /// A move named no legal move: the move as written, cut to 16 bytes.
    Unplayable(Vec<u8>),
}

/// Plays the main line of the game `text` holds. `visit` is called with each
/// position the line reaches and the move played from it, and at the end with
/// the last position and no move; returning `false` stops the line. `lexer`
/// is reset and reused, so that reading many games allocates nothing per game.
pub fn main_line(text: &[u8], lexer: &mut Lexer, visit: &mut dyn FnMut(&Board, Option<Move>) -> bool) -> LineEnd {
    let mut walk = Walk {
        visit,
        fen: [0; MAX_FEN],
        fen_len: 0,
        fen_bad: false,
        both_lost: false,
        zero_zero: false,
        board: None,
        end: None,
    };
    lexer.reset(0);
    lexer.feed(text, &mut walk);
    lexer.finish(&mut walk, false);
    if let Some(end) = walk.end {
        return end;
    }
    if !walk.start() {
        return LineEnd::BadStart;
    }
    if let Some(board) = &walk.board {
        (walk.visit)(board, None);
    }
    LineEnd::End
}

struct Walk<'a> {
    visit: &'a mut dyn FnMut(&Board, Option<Move>) -> bool,
    fen: [u8; MAX_FEN],
    fen_len: usize,
    fen_bad: bool,
    /// The `Result` tag is `0-0`: the movetext's last `0-0` is then the
    /// result, and any other a castling.
    both_lost: bool,
    /// Such a `0-0`, not played until a move follows it.
    zero_zero: bool,
    board: Option<Board>,
    end: Option<LineEnd>,
}

impl Walk<'_> {
    /// Sets up the board at the first move, from the `FEN` tag or the
    /// standard start; whether it is set up.
    fn start(&mut self) -> bool {
        if self.board.is_none() {
            let board = match (self.fen_bad, self.fen_len) {
                (true, _) => None,
                (false, 0) => Some(Board::startpos()),
                (false, n) => std::str::from_utf8(&self.fen[..n]).ok().and_then(|f| Board::from_fen(f.trim()).ok()),
            };
            match board {
                Some(board) => self.board = Some(board),
                None => self.end = Some(LineEnd::BadStart),
            }
        }
        self.board.is_some()
    }

    /// Plays the move `s` names, or ends the line where it names none.
    fn play(&mut self, s: &[u8]) {
        if self.end.is_some() || !self.start() {
            return;
        }
        let Some(board) = self.board.as_mut() else { return };
        match parse_san(board, s) {
            Some(mv) => {
                if (self.visit)(board, Some(mv)) {
                    board.play_unchecked(mv);
                } else {
                    self.end = Some(LineEnd::Stopped);
                }
            }
            None => {
                (self.visit)(board, None);
                self.end =
                    Some(if matches!(s, b"--" | b"Z0") { LineEnd::NullMove } else { LineEnd::Unplayable(s.to_vec()) });
            }
        }
    }

    /// A move after a pending `0-0`: that was a castling.
    fn castled(&mut self) {
        if std::mem::take(&mut self.zero_zero) {
            self.play(b"0-0");
        }
    }
}

impl Sink for Walk<'_> {
    fn tag(&mut self, _: u64, _: u64, name: &[u8], value: &[u8], _: bool) {
        if self.board.is_some() {
            return;
        }
        match name {
            b"FEN" => {
                self.fen_bad = value.len() > MAX_FEN;
                if !self.fen_bad {
                    self.fen[..value.len()].copy_from_slice(value);
                    self.fen_len = value.len();
                }
            }
            b"Result" => self.both_lost = is_both_lost(Some(value)),
            _ => {}
        }
    }

    fn movetext(&mut self, _: u64, _: u64, depth: u32, token: Token<'_>, _: bool) {
        if depth != 0 {
            return;
        }
        let s = match token {
            Token::Symbol(s) => s,
            Token::Star => {
                self.castled();
                return;
            }
            Token::Comment | Token::Other | Token::Skipped => return,
        };
        if s.iter().all(u8::is_ascii_digit) {
            return;
        }
        if s == b"0-0" && self.both_lost {
            self.castled();
            self.zero_zero = true;
            return;
        }
        self.castled();
        if ResultCode::ending(s).is_none() {
            self.play(s);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn play(text: &str) -> (Vec<String>, LineEnd) {
        let mut seen = Vec::new();
        let end = main_line(text.as_bytes(), &mut Lexer::new(), &mut |board, mv| {
            seen.push(mv.map_or_else(|| format!("end {}", board.fen()), |mv| mv.to_string()));
            true
        });
        (seen, end)
    }

    #[test]
    fn plays_the_main_line_from_the_start_or_a_fen() {
        let (seen, end) = play("[Event \"x\"]\n\n1. e4 e5 (1... c5 2. Nf3) 2. Nf3 {c} $1 Nc6! 1-0");
        assert_eq!(seen[..4], ["e2e4", "e7e5", "g1f3", "b8c6"]);
        assert!(seen[4].starts_with("end r1bqkbnr/pppp1ppp/2n5/4p3/4P3/5N2"), "{seen:?}");
        assert_eq!(end, LineEnd::End);
        let (seen, _) = play("[FEN \"4k3/8/8/8/8/8/8/4K2R w K - 0 1\"]\n\n1. O-O Kd7 *");
        assert_eq!(seen[..2], ["e1h1", "e8d7"]);
        // A game without moves reaches its start.
        let (seen, end) = play("[Event \"x\"]\n\n*");
        assert_eq!((seen.len(), end), (1, LineEnd::End));
    }

    #[test]
    fn how_lines_end() {
        assert_eq!(play("1. d4 -- 2. c4 *").1, LineEnd::NullMove);
        let (seen, end) = play("1. e4 Ke7 2. d4 *");
        assert_eq!((seen.len(), end), (2, LineEnd::Unplayable(b"Ke7".to_vec())));
        assert_eq!(play("[FEN \"not a position\"]\n\n1. e4 *").1, LineEnd::BadStart);
        // `0-0` ends a game whose result is both lost, and castles elsewhere.
        let (seen, _) = play("[Result \"0-0\"]\n\n1. e4 e5 2. Nf3 Nf6 3. Bc4 Bc5 0-0");
        assert_eq!(seen.len(), 7);
        // In such a game an earlier `0-0` castles.
        let (seen, _) = play("[Result \"0-0\"]\n\n1. e4 e5 2. Nf3 Nc6 3. Bc4 Bc5 4. 0-0 Nf6 5. d3 0-0");
        assert_eq!(seen.len(), 10);
        assert_eq!(seen[6], "e1h1");
        let (seen, _) = play("1. e4 e5 2. Nf3 Nf6 3. Bc4 Bc5 4. 0-0 *");
        assert_eq!(seen[6], "e1h1");
        let mut n = 0;
        let end = main_line(b"1. e4 e5 2. Nf3", &mut Lexer::new(), &mut |_, _| {
            n += 1;
            n < 2
        });
        assert_eq!((n, end), (2, LineEnd::Stopped));
    }
}
