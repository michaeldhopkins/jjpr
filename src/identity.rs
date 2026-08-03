//! Ownership identity: which commit authors and forge logins count as "you".
//!
//! jj's built-in `mine()` matches a single local `user.email`. A user with the
//! same forge account but different commit emails per machine needs all of
//! their identities recognized. [`Identity`] models that set and produces the
//! ownership revset every discovery path keys off.

/// The identities that count as the current user.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Identity {
    /// Author emails that are yours. Drives the ownership revset, so it governs
    /// discovery in every command (submit/watch/merge and status).
    pub emails: Vec<String>,
    /// Forge logins that are yours. A display-only supplement: used to
    /// attribute a PR to you when its commit email isn't in `emails`.
    pub logins: Vec<String>,
}

impl Identity {
    /// A jj revset matching commits authored by any of your emails —
    /// `author(exact:"e1") | author(exact:"e2") | …`. `author(exact:…)` matches
    /// the email component exactly (no substring collision). Falls back to the
    /// built-in `mine()` when no emails are known, so it is byte-for-byte the
    /// current behavior for a single-identity user.
    pub fn owned_revset(&self) -> String {
        if self.emails.is_empty() {
            return "mine()".to_string();
        }
        self.emails
            .iter()
            .map(|email| format!(r#"author(exact:"{}")"#, escape_revset_string(email)))
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// The free, no-network seed: your local `user.email` plus any configured
    /// emails/logins, deduped. jjpr may later extend it lazily with forge data.
    pub fn seed(local_email: &str, config_emails: &[String], config_logins: &[String]) -> Identity {
        let mut identity = Identity::default();
        identity.push_email(local_email);
        for email in config_emails {
            identity.push_email(email);
        }
        for login in config_logins {
            identity.push_login(login);
        }
        identity
    }

    /// Add author emails (e.g. lazily fetched from the forge), deduped.
    pub fn extend_emails(&mut self, emails: impl IntoIterator<Item = String>) {
        for email in emails {
            self.push_email(&email);
        }
    }

    /// Add a forge login (e.g. the authenticated user), deduped.
    pub fn add_login(&mut self, login: &str) {
        self.push_login(login);
    }

    /// Whether `login` is one of yours — the display-only attribution signal.
    pub fn owns_login(&self, login: &str) -> bool {
        !login.is_empty() && self.logins.iter().any(|l| l == login)
    }

    fn push_email(&mut self, email: &str) {
        if !email.is_empty() && !self.emails.iter().any(|e| e == email) {
            self.emails.push(email.to_string());
        }
    }

    fn push_login(&mut self, login: &str) {
        if !login.is_empty() && !self.logins.iter().any(|l| l == login) {
            self.logins.push(login.to_string());
        }
    }
}

/// Escape a string for embedding inside a jj revset double-quoted literal.
/// Backslash first, then quote, so an already-present backslash isn't
/// double-counted.
fn escape_revset_string(s: &str) -> String {
    s.replace('\\', r"\\").replace('"', "\\\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owned_revset_single_email() {
        let id = Identity {
            emails: vec!["me@x.com".to_string()],
            logins: vec![],
        };
        assert_eq!(id.owned_revset(), r#"author(exact:"me@x.com")"#);
    }

    #[test]
    fn owned_revset_unions_multiple_emails() {
        let id = Identity {
            emails: vec!["a@x.com".to_string(), "b@x.com".to_string()],
            logins: vec![],
        };
        assert_eq!(
            id.owned_revset(),
            r#"author(exact:"a@x.com") | author(exact:"b@x.com")"#
        );
    }

    #[test]
    fn owned_revset_empty_falls_back_to_mine() {
        assert_eq!(Identity::default().owned_revset(), "mine()");
    }

    #[test]
    fn owned_revset_escapes_quote_and_backslash() {
        // Defensive: emails almost never contain these, but the revset literal
        // is the injection boundary.
        let id = Identity {
            emails: vec![r#"a"b\c@x.com"#.to_string()],
            logins: vec![],
        };
        assert_eq!(id.owned_revset(), r#"author(exact:"a\"b\\c@x.com")"#);
    }

    #[test]
    fn escape_handles_backslash_then_quote() {
        assert_eq!(escape_revset_string(r#"a\b"c"#), r#"a\\b\"c"#);
    }

    #[test]
    fn seed_is_local_email_plus_config_deduped() {
        // Local email repeated in config must not duplicate.
        let id = Identity::seed(
            "me@x.com",
            &["me@x.com".to_string(), "work@x.com".to_string()],
            &["my-alt".to_string()],
        );
        assert_eq!(id.emails, vec!["me@x.com", "work@x.com"]);
        assert_eq!(id.logins, vec!["my-alt"]);
    }

    #[test]
    fn seed_ignores_empty_local_email() {
        let id = Identity::seed("", &["work@x.com".to_string()], &[]);
        assert_eq!(id.emails, vec!["work@x.com"]);
    }

    #[test]
    fn extend_emails_and_add_login_dedupe() {
        let mut id = Identity::seed("me@x.com", &[], &[]);
        id.extend_emails(["me@x.com".to_string(), "fetched@x.com".to_string()]);
        assert_eq!(id.emails, vec!["me@x.com", "fetched@x.com"]);
        id.add_login("octocat");
        id.add_login("octocat");
        assert_eq!(id.logins, vec!["octocat"]);
    }

    #[test]
    fn owns_login_matches_known_nonempty_logins() {
        let mut id = Identity::default();
        id.add_login("octocat");
        assert!(id.owns_login("octocat"));
        assert!(!id.owns_login("someone-else"));
        assert!(!id.owns_login(""));
    }
}
