//! Errors a write added: errors before the write are moved to their lines after it and compared.

use std::ops::Range;

/// One place a write changed: 1-based lines `old` of the file before became lines `new` after (both
/// half-open, so an insertion has an empty `old`).
#[derive(Debug, Clone, PartialEq)]
pub struct Hunk {
    pub old: Range<usize>,
    pub new: Range<usize>,
}

/// An error at a 1-based line.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub line: usize,
    pub message: String,
}

/// Where line `line` of the file before a write is after it, or `None` when the write rewrote it.
/// `hunks` are in file order.
pub fn moved(hunks: &[Hunk], line: usize) -> Option<usize> {
    let mut shift = 0isize;
    for hunk in hunks.iter().take_while(|h| h.old.start <= line) {
        if hunk.old.contains(&line) {
            return None;
        }
        shift += hunk.new.len() as isize - hunk.old.len() as isize;
    }
    Some((line as isize + shift) as usize)
}

/// The errors after a write that were not there before: an error before is moved to its line after the
/// write by `moved` and matches one with the same message there; one on a rewritten line matches nothing.
pub fn new_errors(
    before: &[Diagnostic],
    after: Vec<Diagnostic>,
    moved: impl Fn(usize) -> Option<usize>,
) -> Vec<Diagnostic> {
    let before: Vec<Diagnostic> =
        before.iter().filter_map(|d| Some(Diagnostic { line: moved(d.line)?, message: d.message.clone() })).collect();
    after.into_iter().filter(|d| !before.contains(d)).collect()
}

/// Where lines `first..=last` of the file before a write are after it; a rewritten line takes the lines that
/// replaced it.
pub fn moved_lines(hunks: &[Hunk], first: usize, last: usize) -> (usize, usize) {
    let rewritten = |line: usize| hunks.iter().find(|h| h.old.contains(&line));
    let start = moved(hunks, first).or_else(|| rewritten(first).map(|h| h.new.start)).unwrap_or(first);
    let end = moved(hunks, last).or_else(|| rewritten(last).map(|h| h.new.end.saturating_sub(1))).unwrap_or(last);
    (start, end.max(start))
}

/// The places where the lines of `after` differ from those of `before`, in file order.
pub fn hunks(before: &str, after: &str) -> Vec<Hunk> {
    let mut found: Vec<Hunk> = vec![];
    for op in similar::TextDiff::from_lines(before, after).ops() {
        if op.tag() == similar::DiffTag::Equal {
            continue;
        }
        let (old, new) = (op.old_range(), op.new_range());
        let hunk = Hunk { old: old.start + 1..old.end + 1, new: new.start + 1..new.end + 1 };
        // A replacement can come as a deletion next to an insertion.
        match found.last_mut() {
            Some(last) if last.old.end == hunk.old.start && last.new.end == hunk.new.start => {
                last.old.end = hunk.old.end;
                last.new.end = hunk.new.end;
            }
            _ => found.push(hunk),
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(line: usize, message: &str) -> Diagnostic {
        Diagnostic { line, message: message.into() }
    }

    /// Lines 3-4 became lines 3-5: one line longer.
    const GROWN: [Hunk; 1] = [Hunk { old: 3..5, new: 3..6 }];

    #[test]
    fn line_before_edit_stays() {
        // Given / When: 書き換えた箇所より前の行を移す / Then: 同じ行にある
        assert_eq!(moved(&GROWN, 2), Some(2));
    }

    #[test]
    fn line_after_edit_moves_by_lines_added() {
        // Given / When: 書き換えた箇所より後の行を移す / Then: 増えた行数だけ移る
        assert_eq!(moved(&GROWN, 5), Some(6));
    }

    #[test]
    fn rewritten_line_has_no_line_after() {
        // Given / When: 書き換えた行を移す / Then: 書き込み後の行を持たない
        assert_eq!(moved(&GROWN, 4), None);
    }

    #[test]
    fn same_message_on_moved_line_is_not_new() {
        // Given: 5 行目のエラーが、書き込みで 6 行目に移った
        let before = [error(5, "cannot find x")];
        // When: 新しいエラーを求める
        let new = new_errors(&before, vec![error(6, "cannot find x")], |l| moved(&GROWN, l));
        // Then: 新しくない
        assert!(new.is_empty(), "{new:?}");
    }

    #[test]
    fn message_absent_before_is_new() {
        // Given / When: 前にない文のエラーがある
        let new = new_errors(&[], vec![error(1, "mismatched types")], |l| moved(&GROWN, l));
        // Then: 新しい
        assert_eq!(new, [error(1, "mismatched types")]);
    }

    #[test]
    fn same_message_on_other_line_is_new() {
        // Given: 5 行目にあったのと同じ文のエラーが、移した先の 6 行目でなく 1 行目にもできた
        let before = [error(5, "cannot find x")];
        // When: 新しいエラーを求める
        let new = new_errors(&before, vec![error(1, "cannot find x"), error(6, "cannot find x")], |l| moved(&GROWN, l));
        // Then: 1 行目のものが新しい
        assert_eq!(new, [error(1, "cannot find x")]);
    }

    #[test]
    fn error_on_rewritten_line_before_is_not_compared() {
        // Given: 書き換えた 4 行目にあったエラーと、書き換え後の 4 行目にある同じ文のエラー
        let before = [error(4, "cannot find x")];
        // When: 新しいエラーを求める
        let new = new_errors(&before, vec![error(4, "cannot find x")], |l| moved(&GROWN, l));
        // Then: 前のエラーは比較に使われず、書き換え後のエラーは新しい
        assert_eq!(new, [error(4, "cannot find x")]);
    }

    #[test]
    fn lines_after_each_of_two_edits_move_by_lines_added_before_them() {
        // Given: 2 行目が 2 行に、6 行目が消えた書き込み
        let hunks = [Hunk { old: 2..3, new: 2..4 }, Hunk { old: 6..7, new: 7..7 }];
        // When / Then: 2 つの箇所の間の行は 1 行、後ろの行は差し引き 0 行移る
        assert_eq!(moved(&hunks, 4), Some(5));
        assert_eq!(moved(&hunks, 8), Some(8));
    }

    #[test]
    fn line_diff_of_two_texts_gives_rewritten_places() {
        // Given / When: 2 行目が 2 行に変わり、4 行目が消えたテキストの差分をとる
        let found = hunks("a\nb\nc\nd\ne\n", "a\nb1\nb2\nc\ne\n");
        // Then: 2 行目が 2-3 行目に、4 行目がなくなった箇所になる
        assert_eq!(found, [Hunk { old: 2..3, new: 2..4 }, Hunk { old: 4..5, new: 5..5 }]);
    }

    #[test]
    fn moved_lines_of_rewritten_lines_are_the_lines_replacing_them() {
        // Given: 3-4 行目が 3-5 行目になった書き換え / When: 2-3 行目を移す
        let span = moved_lines(&GROWN, 2, 3);
        // Then: 書き換えた 3 行目は、それに代わった 3-5 行目まで広がる
        assert_eq!(span, (2, 5));
    }

    #[test]
    fn errors_before_move_through_two_rewrites_in_turn() {
        // Given: 3-4 行目が 3-5 行目になった書き込みと、その後 1 行目の前に 1 行足した整形
        let formatting = [Hunk { old: 1..1, new: 1..2 }];
        let before = [error(7, "x")];
        // When: 前の 7 行目のエラーを、両方を通して移して比べる
        let found = new_errors(&before, vec![error(9, "x")], |l| moved(&GROWN, l).and_then(|l| moved(&formatting, l)));
        // Then: 7 行目は書き込みで 8 行目、整形で 9 行目に移るので、新しくない
        assert_eq!(found, []);
    }
}
