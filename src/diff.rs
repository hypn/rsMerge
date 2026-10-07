//! Line diff model behind the side-by-side view: aligned rows, difference blocks,
//! and word-level changes within a line.

use similar::{Algorithm, DiffTag, capture_diff_slices_deadline};
use std::borrow::Cow;
use std::collections::HashMap;
use std::ops::Range;
use std::time::{Duration, Instant};

/// Give up on finding a minimal diff after this long and settle for a coarser one.
const LINE_DIFF_DEADLINE: Duration = Duration::from_secs(2);
const INLINE_DIFF_DEADLINE: Duration = Duration::from_millis(20);

#[derive(Clone, Copy, Default, PartialEq)]
pub struct DiffOptions {
    pub ignore_whitespace: bool,
    pub ignore_case: bool,
}

/// One display row. A side with `None` shows a filler line so both sides stay aligned.
#[derive(Clone, Copy)]
pub struct Row {
    pub lines: [Option<usize>; 2],
    pub block: Option<usize>,
}

/// A contiguous difference: the rows it occupies and the lines it covers on each side
/// (a side's range is empty when the lines exist only on the other side).
pub struct Block {
    pub rows: Range<usize>,
    pub lines: [Range<usize>; 2],
}

#[derive(Default)]
pub struct DiffModel {
    pub rows: Vec<Row>,
    pub blocks: Vec<Block>,
    /// For each side, the row each line is displayed on.
    pub row_of_line: [Vec<usize>; 2],
}

impl DiffModel {
    pub fn compute(left: &[String], right: &[String], options: DiffOptions) -> Self {
        // Diff line ids rather than strings: equal lines share an id, so comparisons are cheap.
        let mut ids = HashMap::new();
        let (left, right) = (intern(&mut ids, left, options), intern(&mut ids, right, options));
        let deadline = Some(Instant::now() + LINE_DIFF_DEADLINE);
        let ops = capture_diff_slices_deadline(Algorithm::Myers, &left, &right, deadline);

        let mut model = DiffModel::default();
        let mut pending: Option<[Range<usize>; 2]> = None;
        for op in ops {
            let (tag, old, new) = op.as_tag_tuple();
            if tag == DiffTag::Equal {
                if let Some(lines) = pending.take() {
                    model.push_block(lines);
                }
                for (l, r) in old.zip(new) {
                    model.push_row([Some(l), Some(r)], None);
                }
            } else {
                pending = Some(match pending {
                    Some([l, r]) => [l.start..old.end, r.start..new.end],
                    None => [old, new],
                });
            }
        }
        if let Some(lines) = pending {
            model.push_block(lines);
        }
        model
    }

    fn push_row(&mut self, lines: [Option<usize>; 2], block: Option<usize>) {
        let row = self.rows.len();
        for (side, line) in lines.iter().enumerate() {
            if line.is_some() {
                self.row_of_line[side].push(row);
            }
        }
        self.rows.push(Row { lines, block });
    }

    fn push_block(&mut self, [left, right]: [Range<usize>; 2]) {
        let block = self.blocks.len();
        let start = self.rows.len();
        for i in 0..left.len().max(right.len()) {
            let l = (i < left.len()).then_some(left.start + i);
            let r = (i < right.len()).then_some(right.start + i);
            self.push_row([l, r], Some(block));
        }
        self.blocks.push(Block { rows: start..self.rows.len(), lines: [left, right] });
    }
}

fn intern<'a>(ids: &mut HashMap<Cow<'a, str>, u32>, lines: &'a [String], options: DiffOptions) -> Vec<u32> {
    lines
        .iter()
        .map(|line| {
            let next = ids.len() as u32;
            *ids.entry(normalize(line, options)).or_insert(next)
        })
        .collect()
}

fn normalize(line: &str, options: DiffOptions) -> Cow<'_, str> {
    let mut line = Cow::Borrowed(line);
    if options.ignore_whitespace {
        line = Cow::Owned(line.chars().filter(|c| !c.is_whitespace()).collect());
    }
    if options.ignore_case {
        line = Cow::Owned(line.to_lowercase());
    }
    line
}

/// Character ranges (in chars, not bytes) that differ between two lines, compared word by word.
pub fn inline_diff(left: &str, right: &str) -> [Vec<Range<usize>>; 2] {
    let (lt, rt) = (tokenize(left), tokenize(right));
    let lw: Vec<&str> = lt.iter().map(|t| t.text).collect();
    let rw: Vec<&str> = rt.iter().map(|t| t.text).collect();
    let deadline = Some(Instant::now() + INLINE_DIFF_DEADLINE);
    let mut out = [Vec::new(), Vec::new()];
    for op in capture_diff_slices_deadline(Algorithm::Myers, &lw, &rw, deadline) {
        let (tag, old, new) = op.as_tag_tuple();
        if tag == DiffTag::Equal {
            continue;
        }
        for (side, tokens, range) in [(0, &lt, old), (1, &rt, new)] {
            if let (Some(first), Some(last)) = (tokens.get(range.start), range.end.checked_sub(1).and_then(|i| tokens.get(i))) {
                add_range(&mut out[side], first.start..last.end);
            }
        }
    }
    out
}

struct Token<'a> {
    text: &'a str,
    start: usize,
    end: usize,
}

/// Splits a line into words, runs of whitespace, and single punctuation characters.
fn tokenize(line: &str) -> Vec<Token<'_>> {
    #[derive(PartialEq)]
    enum Class {
        Word,
        Space,
        Other,
    }
    let class = |c: char| {
        if c.is_alphanumeric() || c == '_' {
            Class::Word
        } else if c.is_whitespace() {
            Class::Space
        } else {
            Class::Other
        }
    };

    let mut tokens: Vec<Token> = Vec::new();
    let mut prev: Option<Class> = None;
    let mut token_byte = 0;
    for (col, (byte, c)) in line.char_indices().enumerate() {
        let cls = class(c);
        let joins = prev.as_ref() == Some(&cls) && cls != Class::Other;
        if joins {
            let t = tokens.last_mut().expect("joins implies a previous token");
            t.end = col + 1;
            t.text = &line[token_byte..byte + c.len_utf8()];
        } else {
            token_byte = byte;
            tokens.push(Token { text: &line[byte..byte + c.len_utf8()], start: col, end: col + 1 });
        }
        prev = Some(cls);
    }
    tokens
}

fn add_range(ranges: &mut Vec<Range<usize>>, range: Range<usize>) {
    match ranges.last_mut() {
        Some(last) if last.end >= range.start => last.end = last.end.max(range.end),
        _ => ranges.push(range),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(s: &str) -> Vec<String> {
        s.lines().map(String::from).collect()
    }

    #[test]
    fn aligns_changes_with_filler_rows() {
        let m = DiffModel::compute(&lines("a\nb\nc"), &lines("a\nB\nx\nc"), DiffOptions::default());
        assert_eq!(m.blocks.len(), 1);
        assert_eq!(m.blocks[0].lines, [1..2, 1..3]);
        assert_eq!(m.rows.len(), 4);
        assert_eq!(m.rows[2].lines, [None, Some(2)]);
        assert_eq!(m.row_of_line[0], vec![0, 1, 3]);
    }

    #[test]
    fn ignore_options() {
        let opts = DiffOptions { ignore_whitespace: true, ignore_case: true };
        let m = DiffModel::compute(&lines("Foo  bar"), &lines("foo bar "), opts);
        assert!(m.blocks.is_empty());
    }

    #[test]
    #[ignore = "timing check: cargo test --release -- --ignored --nocapture"]
    fn large_file_speed() {
        let left: Vec<String> = (0..200_000).map(|i| format!("line {i} of some reasonably long source text;")).collect();
        let mut right = left.clone();
        for i in (0..right.len()).step_by(997) {
            right[i].push_str(" changed");
        }
        let start = Instant::now();
        let m = DiffModel::compute(&left, &right, DiffOptions::default());
        println!("200k lines, {} blocks: {:?}", m.blocks.len(), start.elapsed());
        assert_eq!(m.blocks.len(), right.len().div_ceil(997));
    }

    #[test]
    fn inline_word_ranges() {
        let [l, r] = inline_diff("let x = 1;", "let y = 1;");
        assert_eq!(l, vec![4..5]);
        assert_eq!(r, vec![4..5]);
    }
}
