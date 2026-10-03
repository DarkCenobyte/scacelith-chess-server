//! Presence (the former `presence.js` claims): the live connection of each online account, and
//! the online accounts by name for direct challenges. Owned by the lobby actor.
//!
//! One live connection per account: a claim by a new connection replaces the previous one, which
//! the lobby then kicks. A release counts only when it comes from the live connection, so the
//! close of a replaced connection does not log out its successor.

use std::collections::HashMap;
use std::sync::Arc;

use super::link::ConnLink;
use crate::ids::{ConnId, UserId};

/// The online accounts and their live connections.
#[derive(Debug, Default)]
pub(crate) struct Presence {
    users: HashMap<UserId, Arc<ConnLink>>,
    /// Lower-case username to account.
    by_name: HashMap<String, UserId>,
}

impl Presence {
    /// Makes `link` the live connection of its account. Returns the connection it replaces
    /// (`None` when there was none, or when it is the same connection claiming again).
    pub(crate) fn claim(&mut self, link: Arc<ConnLink>) -> Option<Arc<ConnLink>> {
        let previous = self.users.insert(link.user_id(), link.clone());
        if let Some(prev) = &previous {
            self.forget_name(prev);
        }
        if !link.username().is_empty() {
            self.by_name.insert(link.username().to_lowercase(), link.user_id());
        }
        previous.filter(|prev| prev.conn_id() != link.conn_id())
    }

    /// Forgets the account's connection if it is still `conn`. Returns whether it was.
    pub(crate) fn release(&mut self, user: UserId, conn: ConnId) -> bool {
        if self.users.get(&user).is_none_or(|cur| cur.conn_id() != conn) {
            return false;
        }
        if let Some(cur) = self.users.remove(&user) {
            self.forget_name(&cur);
        }
        true
    }

    fn forget_name(&mut self, link: &ConnLink) {
        if link.username().is_empty() {
            return;
        }
        let name = link.username().to_lowercase();
        if self.by_name.get(&name) == Some(&link.user_id()) {
            self.by_name.remove(&name);
        }
    }

    /// The account's live connection.
    pub(crate) fn get(&self, user: UserId) -> Option<&Arc<ConnLink>> {
        self.users.get(&user)
    }

    /// The online account named `name` (any case).
    pub(crate) fn user_id_by_name(&self, name: &str) -> Option<UserId> {
        self.by_name.get(&name.to_lowercase()).copied()
    }

    /// Online accounts.
    pub(crate) fn len(&self) -> usize {
        self.users.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::realtime::endpoint::Endpoint;

    fn link(user: UserId, conn: ConnId, name: &str) -> Arc<ConnLink> {
        ConnLink::new(Endpoint::for_tests(conn, user).0, name.to_string(), [0; 32]).0
    }

    #[test]
    fn claims_replaces_and_releases_only_the_live_connection() {
        let mut p = Presence::default();
        assert!(p.claim(link(1, 10, "Alice")).is_none());
        assert_eq!(p.user_id_by_name("alice"), Some(1));
        let previous = p.claim(link(1, 20, "Alice")).expect("replaced");
        assert_eq!(previous.conn_id(), 10);
        assert!(!p.release(1, 10), "the replaced connection closing");
        assert_eq!(p.get(1).map(|l| l.conn_id()), Some(20));
        assert!(p.release(1, 20));
        assert!(p.get(1).is_none());
        assert_eq!(p.user_id_by_name("Alice"), None);
        let again = link(3, 5, "");
        assert!(p.claim(again.clone()).is_none());
        assert!(p.claim(again).is_none(), "the same connection again");
        assert_eq!(p.len(), 1);
    }

    #[test]
    fn finds_online_accounts_by_name_in_any_case() {
        let mut p = Presence::default();
        p.claim(link(7, 1, "Émile"));
        assert_eq!(p.user_id_by_name("éMILE"), Some(7));
        assert_eq!(p.user_id_by_name("emile"), None);
        // A renamed account (a new session after a change of username) answers to its new name only.
        p.claim(link(7, 2, "Zoé"));
        assert_eq!(p.user_id_by_name("émile"), None);
        assert_eq!(p.user_id_by_name("ZOÉ"), Some(7));
    }
}
