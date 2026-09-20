//! Text fitted to a terminal's columns.
//!
//! Every list pane has the same problem: a name that is user data, a column
//! that is as wide as the terminal allows, and no room to be wrong about which
//! is bigger. The REPOS and WORKTREES panes had the same function twice, which
//! is how the two would eventually disagree about what fits.

use unicode_width::UnicodeWidthStr;

/// What a shortened string ends with.
pub const ELLIPSIS: &str = "…";

/// Shorten `text` to `room` **display columns**, ending in `…` when it does
/// not fit.
///
/// Columns, not characters. A repo or branch name is usually ASCII, where the
/// two agree — but a CJK name is two columns per character, and counting
/// characters lets the name overrun its pane and push the columns after it off
/// the edge.
pub fn truncate(text: &str, room: usize) -> String {
    if room == 0 {
        return String::new();
    }
    if text.width() <= room {
        return text.to_owned();
    }
    if room < 2 {
        // No room for a character and the ellipsis both.
        return ELLIPSIS.to_owned();
    }
    let budget = room - ELLIPSIS.width();
    let mut kept = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let w = ch.to_string().width();
        if used + w > budget {
            break;
        }
        kept.push(ch);
        used += w;
    }
    format!("{kept}{ELLIPSIS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_that_fits_is_untouched() {
        assert_eq!(truncate("web-app", 10), "web-app");
        assert_eq!(truncate("web-app", 7), "web-app");
    }

    #[test]
    fn text_that_does_not_fit_ends_in_an_ellipsis() {
        assert_eq!(truncate("web-app", 6), "web-a…");
        assert_eq!(truncate("web-app", 1), ELLIPSIS);
        assert_eq!(truncate("web-app", 0), "");
    }

    #[test]
    fn width_is_measured_in_columns_not_characters() {
        // Four characters, eight columns. Counting characters would call this
        // a fit at six and overrun by two.
        let wide = "課題管理";
        assert_eq!(wide.width(), 8);
        assert_eq!(truncate(wide, 8), wide);
        let clipped = truncate(wide, 6);
        assert!(clipped.width() <= 6, "{clipped:?}");
        assert!(clipped.ends_with(ELLIPSIS));
    }

    #[test]
    fn the_result_never_exceeds_its_budget() {
        // The property both panes depend on: whatever comes back fits, for
        // every width and every input, or a column after it is pushed off.
        for text in ["", "a", "web-app", "課題管理", "feat/ABC-4471-long-name"] {
            for room in 0..30usize {
                let out = truncate(text, room);
                assert!(
                    out.width() <= room,
                    "{text:?} at {room}: {out:?} is {} columns",
                    out.width()
                );
            }
        }
    }
}
