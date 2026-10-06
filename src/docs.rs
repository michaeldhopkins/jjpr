//! Links into the published docs, for messages that give up.
//!
//! When jjpr stops because no single command is safe, it points at a section
//! of the recovery page instead. `tests/docs_links.rs` checks that every
//! anchor named here exists in `docs/src/recovering.md`, so a renamed heading
//! fails the build rather than sending users to the top of the page.

/// The published book, as `docs/book.toml`'s `site-url` serves it.
pub const BOOK: &str = "https://michaeldhopkins.com/docs/jjpr";

/// The recovery page, at the section whose mdBook anchor is `anchor`.
pub fn recovering(anchor: &str) -> String {
    format!("{BOOK}/recovering.html#{anchor}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovering_links_a_section_of_the_published_page() {
        assert_eq!(
            recovering("x"),
            "https://michaeldhopkins.com/docs/jjpr/recovering.html#x"
        );
    }
}
