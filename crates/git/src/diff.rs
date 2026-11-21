use std::collections::BTreeSet;
use std::ops::RangeInclusive;

use similar::TextDiff;

/// Nomor baris sisi kanan (file baru) untuk setiap baris `+` di patch. Hanya untuk patch satu file.
pub fn commentable_lines(patch: &str) -> BTreeSet<u32> {
    let mut lines = BTreeSet::new();
    let mut right = 0u32;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(start) = hunk_right_start(line) {
            right = start;
            in_hunk = true;
        } else if !in_hunk {
            continue;
        } else if line.starts_with('+') {
            lines.insert(right);
            right += 1;
        } else if line.starts_with('-') || line.starts_with('\\') {
            // baris lama dan penanda "\ No newline at end of file" tidak menggeser sisi kanan
        } else {
            right += 1; // konteks (termasuk baris kosong yang spasinya terpotong)
        }
    }
    lines
}

/// Jumlah baris `+` di patch. Hanya untuk patch satu file.
pub fn added_line_count(patch: &str) -> usize {
    let mut in_hunk = false;
    let mut count = 0;
    for line in patch.lines() {
        if hunk_right_start(line).is_some() {
            in_hunk = true;
        } else if in_hunk && line.starts_with('+') {
            count += 1;
        }
    }
    count
}

/// Hunk saja (tanpa header `---`/`+++`), context 3. Kosong jika old == new.
pub fn unified_patch(old: &str, new: &str) -> String {
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(3)
        .to_string()
}

/// Ambil nomor baris awal sisi kanan dari `@@ -a,b +c,d @@`.
pub fn hunk_right_start(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split_whitespace().find(|part| part.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
}

/// Rentang nomor baris sisi kanan (`first..=last`) untuk setiap hunk di patch yang memiliki baris sisi kanan.
/// Hunk yang hanya berisi penghapusan dilewati.
pub fn commentable_hunks(patch: &str) -> Vec<RangeInclusive<u32>> {
    let mut hunks = Vec::new();
    let mut current: Option<(u32, u32)> = None;
    for line in patch.lines() {
        if let Some(start) = hunk_right_start(line) {
            if let Some((hunk_start, next_right)) = current.take()
                && next_right > hunk_start
            {
                hunks.push(hunk_start..=(next_right - 1));
            }
            current = Some((start, start));
        } else if let Some((_, next_right)) = &mut current {
            if line.starts_with('-') || line.starts_with('\\') {
                // baris lama dan penanda "\ No newline at end of file" tidak menggeser sisi kanan
            } else {
                *next_right += 1;
            }
        }
    }
    if let Some((hunk_start, next_right)) = current
        && next_right > hunk_start
    {
        hunks.push(hunk_start..=(next_right - 1));
    }
    hunks
}

/// True jika `start_line < end_line` dan keduanya berada di hunk yang sama.
pub fn same_hunk(hunks: &[RangeInclusive<u32>], start_line: u32, end_line: u32) -> bool {
    start_line < end_line
        && hunks
            .iter()
            .any(|h| h.contains(&start_line) && h.contains(&end_line))
}

/// Irisan tidak kosong dari dua daftar rentang hunk.
pub fn intersect_hunks(
    a: &[RangeInclusive<u32>],
    b: &[RangeInclusive<u32>],
) -> Vec<RangeInclusive<u32>> {
    let mut out = Vec::new();
    for x in a {
        for y in b {
            let start = (*x.start()).max(*y.start());
            let end = (*x.end()).min(*y.end());
            if start <= end {
                out.push(start..=end);
            }
        }
    }
    out
}

/// Offset kolom akhir (berbasis-1 dalam unit UTF-16) untuk `target_line` pada sisi kanan patch.
/// Mengembalikan `None` jika `target_line` tidak ditemukan di hunk sisi kanan.
pub fn right_line_end_offset(patch: &str, target_line: u32) -> Option<u32> {
    let mut right = 0u32;
    let mut in_hunk = false;
    for line in patch.lines() {
        if let Some(start) = hunk_right_start(line) {
            right = start;
            in_hunk = true;
        } else if !in_hunk || line.starts_with('-') || line.starts_with('\\') {
            continue;
        } else {
            if right == target_line {
                let code = line
                    .strip_prefix('+')
                    .or_else(|| line.strip_prefix(' '))
                    .unwrap_or(line)
                    .trim_end_matches('\r');
                return u32::try_from(code.encode_utf16().count().saturating_add(1)).ok();
            }
            right += 1;
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::normalize_path;

    #[test]
    fn commentable_lines_counts_right_side() {
        let p = "@@ -1,3 +1,4 @@\n a\n-b\n+B\n+C\n d\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([2, 3]));
        assert_eq!(added_line_count(p), 2);
    }

    #[test]
    fn multiple_hunks() {
        let p = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,3 @@\n x\n+y\n z\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([2, 11]));
    }

    #[test]
    fn no_newline_marker_ignored() {
        let p = "@@ -1,2 +1,2 @@\n a\n-b\n\\ No newline at end of file\n+B\n\\ No newline at end of file\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([2]));
        assert_eq!(added_line_count(p), 1);
    }

    #[test]
    fn hunk_header_without_counts() {
        let p = "@@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([1]));
        assert_eq!(added_line_count(p), 1);
    }

    #[test]
    fn plus_and_minus_prefixed_content_inside_hunk() {
        // "+++x" adalah baris tambahan berisi "++x"; "---x" adalah baris yang dihapus.
        let p = "@@ -1,2 +1,2 @@\n---x\n+++x\n a\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([1]));
        assert_eq!(added_line_count(p), 1);
    }

    #[test]
    fn empty_context_line_counts() {
        // Baris kosong tanpa spasi di depan tetap konteks.
        let p = "@@ -1,3 +1,3 @@\n a\n\n-b\n+B\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([3]));
    }

    #[test]
    fn crlf_patch() {
        let p = "@@ -1,2 +1,2 @@\r\n a\r\n-b\r\n+B\r\n";
        assert_eq!(commentable_lines(p), BTreeSet::from([2]));
        assert_eq!(added_line_count(p), 1);
    }

    #[test]
    fn unified_patch_roundtrip() {
        let p = unified_patch("a\nb\n", "a\nB\n");
        assert!(p.starts_with("@@"));
        assert_eq!(commentable_lines(&p), BTreeSet::from([2]));
    }

    #[test]
    fn unified_patch_without_trailing_newline() {
        let p = unified_patch("a\nb", "a\nB");
        assert_eq!(commentable_lines(&p), BTreeSet::from([2]));
        assert_eq!(added_line_count(&p), 1);
    }

    #[test]
    fn unified_patch_identical_is_empty() {
        assert_eq!(unified_patch("a\n", "a\n"), "");
    }

    #[test]
    fn normalize_strips_leading_slash() {
        assert_eq!(normalize_path("/src/a.rs"), "src/a.rs");
    }

    #[test]
    fn commentable_hunks_handles_single_multiple_deletion_and_crlf() {
        let single = "@@ -1,3 +1,4 @@\n a\n-b\n+B\n+C\n d\n";
        assert_eq!(commentable_hunks(single), vec![1..=4]);

        let multi = "@@ -1,2 +1,2 @@\n a\n-b\n+B\n@@ -10,2 +10,3 @@\n x\n+y\n z\n";
        assert_eq!(commentable_hunks(multi), vec![1..=2, 10..=12]);

        let pure_delete = "@@ -1,2 +0,0 @@\n-a\n-b\n@@ -5,1 +3,2 @@\n c\n+d\n";
        assert_eq!(commentable_hunks(pure_delete), vec![3..=4]);

        let crlf = "@@ -1,2 +1,2 @@\r\n a\r\n-b\r\n+B\r\n";
        assert_eq!(commentable_hunks(crlf), vec![1..=2]);
    }

    #[test]
    fn same_hunk_checks_bounds_and_ordering() {
        let hunks = vec![1..=4, 10..=12];
        assert!(same_hunk(&hunks, 1, 3)); // context start_line to + line
        assert!(same_hunk(&hunks, 10, 12));
        assert!(!same_hunk(&hunks, 2, 11)); // across hunks
        assert!(!same_hunk(&hunks, 2, 2)); // equal
        assert!(!same_hunk(&hunks, 3, 2)); // inverted
        assert!(!same_hunk(&hunks, 5, 6)); // outside hunks
    }

    #[test]
    fn intersect_hunks_handles_overlap_split_and_disjoint() {
        use std::slice::from_ref;

        assert_eq!(
            intersect_hunks(from_ref(&(1..=10)), from_ref(&(3..=7))),
            vec![3..=7]
        );
        assert_eq!(
            intersect_hunks(from_ref(&(1..=5)), from_ref(&(4..=9))),
            vec![4..=5]
        );
        assert_eq!(
            intersect_hunks(from_ref(&(1..=20)), &[2..=5, 10..=15]),
            vec![2..=5, 10..=15]
        );
        assert_eq!(
            intersect_hunks(from_ref(&(1..=4)), from_ref(&(5..=8))),
            Vec::<RangeInclusive<u32>>::new()
        );
    }

    #[test]
    fn right_line_end_offset_handles_utf16_context_empty_and_crlf() {
        let patch =
            "--- a/f\n+++ b/f\n@@ -1,4 +1,6 @@\n ctx\n-deleted line\n+let x = 1;\n+\n+😀\r\n\n";
        assert_eq!(right_line_end_offset(patch, 1), Some(4)); // "ctx" -> 3 + 1
        assert_eq!(right_line_end_offset(patch, 2), Some(11)); // "let x = 1;" -> 10 + 1
        assert_eq!(right_line_end_offset(patch, 3), Some(1)); // empty added line "+" -> 0 + 1
        assert_eq!(right_line_end_offset(patch, 4), Some(3)); // emoji surrogate pair = 2 UTF-16 units -> 2 + 1
        assert_eq!(right_line_end_offset(patch, 5), Some(1)); // stripped empty context line -> 0 + 1
        assert_eq!(right_line_end_offset(patch, 6), None);
    }
}
