use std::collections::BTreeSet;

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
fn hunk_right_start(line: &str) -> Option<u32> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split_whitespace().find(|part| part.starts_with('+'))?;
    plus[1..].split(',').next()?.parse().ok()
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
}
