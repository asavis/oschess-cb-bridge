//! A game's move tree, built by replaying it and written as movetext in PGN
//! order.

use std::fmt::Write;

use chesscore::{Board, Color as CColor, Move};

use super::san::{write_check_suffix, write_san_body};
use crate::replay::TreeVisitor;

const NONE: u32 = u32::MAX;

/// A move of the tree. Children form a linked list, and every SAN lives in one
/// shared buffer, so building the tree allocates nothing per move.
struct Node {
    san: (u32, u32),
    fullmove: u16,
    white: bool,
    first_child: u32,
    last_child: u32,
    next_sibling: u32,
}

impl Node {
    fn new(san: (u32, u32), fullmove: u16, white: bool) -> Node {
        Node { san, fullmove, white, first_child: NONE, last_child: NONE, next_sibling: NONE }
    }
}

pub(super) struct TreeBuilder {
    nodes: Vec<Node>,
    sans: String,
    cur: u32,
    parent_of_last: u32,
    branches: Vec<u32>,
}

impl TreeBuilder {
    pub(super) fn new() -> Self {
        TreeBuilder {
            nodes: vec![Node::new((0, 0), 0, true)],
            sans: String::new(),
            cur: 0,
            parent_of_last: 0,
            branches: Vec::new(),
        }
    }

    /// A capacity guess for the movetext.
    pub(super) fn text_len(&self) -> usize {
        self.sans.len() + 4 * self.nodes.len()
    }

    /// The main line's last move in stored order, when the game has a move.
    pub(super) fn last_main_move(&self) -> Option<u32> {
        let mut n = 0;
        while self.nodes[n as usize].first_child != NONE {
            n = self.nodes[n as usize].first_child;
        }
        // Node 0 is the root; the moves are numbered from 1 as they were played.
        n.checked_sub(1)
    }
}

impl TreeVisitor for TreeBuilder {
    fn play(&mut self, before: &Board, mv: Option<Move>, _main_line: bool) {
        let start = self.sans.len() as u32;
        match mv {
            Some(mv) => write_san_body(&mut self.sans, before, mv),
            None => self.sans.push_str("--"),
        }
        let white = before.side_to_move() == CColor::White;
        let id = self.nodes.len() as u32;
        self.nodes.push(Node::new((start, self.sans.len() as u32), before.fullmove_number(), white));
        let cur = self.cur as usize;
        match self.nodes[cur].last_child {
            NONE => self.nodes[cur].first_child = id,
            last => self.nodes[last as usize].next_sibling = id,
        }
        self.nodes[cur].last_child = id;
        self.parent_of_last = self.cur;
        self.cur = id;
    }
    fn played(&mut self, after: &Board) {
        // The suffix extends the SAN just written, which ends the buffer.
        write_check_suffix(&mut self.sans, after);
        self.nodes[self.cur as usize].san.1 = self.sans.len() as u32;
    }
    fn branch(&mut self) {
        self.branches.push(self.parent_of_last);
    }
    fn resume(&mut self) {
        // walk() calls resume only with a branch outstanding.
        self.cur = self.branches.pop().unwrap_or(0);
    }
}

/// Where a move stands in the tree, in both of the orders annotation
/// positions count in.
#[derive(Clone, Copy)]
pub(super) struct At {
    /// In PGN order: the order a PGN lists the moves, each alternative and
    /// everything after it right after the move it replaces.
    pub(super) pgn: u32,
    /// In stored order: the order the moves were played into the tree, depth
    /// first with the main line first at every position.
    pub(super) stored: u32,
}

/// What goes around each move.
pub(super) trait Notes {
    /// Writes what comes before the move, ending in a space, and says whether
    /// it wrote anything: the move then repeats its number.
    fn before(&mut self, at: At, out: &mut String) -> bool;
    /// Writes what follows the move, each part after a space, and says whether
    /// it ended in a comment: a black move after it then repeats its number.
    fn after(&mut self, at: At, out: &mut String) -> bool;
}

/// No annotations.
pub(super) struct Bare;

impl Notes for Bare {
    fn before(&mut self, _at: At, _out: &mut String) -> bool {
        false
    }
    fn after(&mut self, _at: At, _out: &mut String) -> bool {
        false
    }
}

/// Writes the tree below the root as movetext. Iterative, so that however
/// deeply the variations nest, the depth costs heap and not stack.
pub(super) fn emit(tree: &TreeBuilder, notes: &mut impl Notes, out: &mut String) {
    enum Step {
        /// Continue the line whose last written move is `node`.
        Line {
            node: u32,
            force_number: bool,
        },
        /// Write the alternative `alt` to the main move `main`, and those after it.
        Alternatives {
            main: u32,
            alt: u32,
        },
        Close,
    }
    let nodes = &tree.nodes;
    let mut index = 0u32;
    // Writes move `n`, its number when due and its notes; says whether a
    // comment followed it.
    let mut write = |out: &mut String, n: u32, force_number: bool| -> bool {
        // Node 0 is the root; the moves are numbered from 1 as they were played.
        let at = At { pgn: index, stored: n - 1 };
        index += 1;
        let force_number = notes.before(at, out) || force_number;
        let n = &nodes[n as usize];
        // Writing to a String cannot fail.
        let _ = if n.white {
            write!(out, "{}. ", n.fullmove)
        } else if force_number {
            write!(out, "{}... ", n.fullmove)
        } else {
            Ok(())
        };
        out.push_str(&tree.sans[n.san.0 as usize..n.san.1 as usize]);
        let commented = notes.after(at, out);
        out.push(' ');
        commented
    };
    let mut steps = vec![Step::Line { node: 0, force_number: true }];
    while let Some(step) = steps.pop() {
        match step {
            Step::Line { node, force_number } => {
                let main = nodes[node as usize].first_child;
                if main == NONE {
                    continue;
                }
                let commented = write(out, main, force_number);
                match nodes[main as usize].next_sibling {
                    NONE => steps.push(Step::Line { node: main, force_number: commented }),
                    alt => steps.push(Step::Alternatives { main, alt }),
                }
            }
            Step::Alternatives { main, alt } => {
                if alt == NONE {
                    // Back on the main line after its alternatives: repeat the number.
                    steps.push(Step::Line { node: main, force_number: true });
                } else {
                    steps.push(Step::Alternatives { main, alt: nodes[alt as usize].next_sibling });
                    steps.push(Step::Close);
                    out.push('(');
                    let commented = write(out, alt, true);
                    steps.push(Step::Line { node: alt, force_number: commented });
                }
            }
            Step::Close => {
                if out.ends_with(' ') {
                    out.pop();
                }
                out.push_str(") ");
            }
        }
    }
}
