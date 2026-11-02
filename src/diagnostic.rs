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
/// write and matches one with the same message there; one on a rewritten line matches nothing.
pub fn new_errors(before: &[Diagnostic], after: Vec<Diagnostic>, hunks: &[Hunk]) -> Vec<Diagnostic> {
    let before: Vec<Diagnostic> = before
        .iter()
        .filter_map(|d| Some(Diagnostic { line: moved(hunks, d.line)?, message: d.message.clone() }))
        .collect();
    after.into_iter().filter(|d| !before.contains(d)).collect()
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
        let new = new_errors(&before, vec![error(6, "cannot find x")], &GROWN);
        // Then: 新しくない
        assert!(new.is_empty(), "{new:?}");
    }

    #[test]
    fn message_absent_before_is_new() {
        // Given / When: 前にない文のエラーがある
        let new = new_errors(&[], vec![error(1, "mismatched types")], &GROWN);
        // Then: 新しい
        assert_eq!(new, [error(1, "mismatched types")]);
    }

    #[test]
    fn same_message_on_other_line_is_new() {
        // Given: 5 行目にあったのと同じ文のエラーが、移した先の 6 行目でなく 1 行目にもできた
        let before = [error(5, "cannot find x")];
        // When: 新しいエラーを求める
        let new = new_errors(&before, vec![error(1, "cannot find x"), error(6, "cannot find x")], &GROWN);
        // Then: 1 行目のものが新しい
        assert_eq!(new, [error(1, "cannot find x")]);
    }

    #[test]
    fn error_on_rewritten_line_before_is_not_compared() {
        // Given: 書き換えた 4 行目にあったエラーと、書き換え後の 4 行目にある同じ文のエラー
        let before = [error(4, "cannot find x")];
        // When: 新しいエラーを求める
        let new = new_errors(&before, vec![error(4, "cannot find x")], &GROWN);
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
}
