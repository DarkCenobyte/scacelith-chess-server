//! Sessions: creation, validation through a cache, revocation and its notification.
//!
//! A session token is `sct_` + 43 base64url characters (32 random bytes); only its SHA-256 (lower
//! hex) is stored. [`Sessions::validate`] keeps what it read for at most 30 s in a bounded LRU
//! cache (positive and negative answers), checks revocation, the absolute expiry
//! (`SESSION_MAX_DAYS`) and the idle expiry (`SESSION_IDLE_DAYS`), and writes the last use at most
//! every 5 minutes (sliding the idle expiry). A revocation through the API drops the cache entries
//! at once; one written by another process (the admin CLI) is seen within 30 s.
//!
//! Revocations are told to the realtime layer through [`SessionEvents`]: the listed sessions'
//! connections are closed, or, without a list ("every session of the user"), the user's
//! connection. A refresh (an account change that the cache holds: the address confirmed or
//! changed) only drops the cached entries of the user; it closes nothing.

use std::sync::Arc;

use parking_lot::Mutex;
use serde_json::{Value, json};

use super::SessionInfo;
use super::error::AuthResult;
use super::tokens::{SESSION_PREFIX, is_prefixed_token};
use crate::clock::SharedClock;
use crate::config::Config;
use crate::events::SessionEvents;
use crate::ids::UserId;
use crate::log::{self, Logger};
use crate::log_warn;
use crate::security::keys::{random_token, sha256_hex};
use crate::security::ratelimit::LruMap;
use crate::store::{self, NewSession, Store, User, UserStatus};

/// How long a validation answer is kept.
pub const SESSION_CACHE_TTL_MS: i64 = 30_000;
/// How often, at most, the last use of a session is written.
pub const SESSION_TOUCH_EVERY_MS: i64 = 5 * 60_000;
/// Sessions kept in the validation cache at most.
pub const SESSION_CACHE_SIZE: usize = 10_000;
/// Loads of a session retried when invalidations land during them.
const LOAD_ATTEMPTS: usize = 4;

const DAY_MS: i64 = 86_400_000;

/// A session as the cache holds it.
#[derive(Clone, Debug)]
struct Cached {
    id: i64,
    user_id: UserId,
    username: String,
    email_verified: bool,
    last_seen_at: i64,
    expires_at: i64,
    idle_expires_at: i64,
}

#[derive(Clone, Debug)]
struct Entry {
    loaded_at: i64,
    /// `None`: no usable session (unknown, revoked, or its account is not active).
    session: Option<Cached>,
}

struct Cache {
    map: LruMap<Entry>,
    /// Incremented by every invalidation: a load that saw it change may hold what was just
    /// revoked, and is read again.
    epoch: u64,
}

/// A session just opened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NewSessionToken {
    /// The token (given once to the client).
    pub token: String,
    /// Its SHA-256, lower hex.
    pub token_hash: String,
    /// Absolute expiry.
    pub expires_at: i64,
    /// The session's id.
    pub session_id: i64,
}

/// The session manager (module documentation).
pub(crate) struct Sessions {
    store: Store,
    clock: SharedClock,
    log: Logger,
    events: Arc<dyn SessionEvents>,
    idle_ms: i64,
    max_ms: i64,
    max_per_user: i64,
    cache: Mutex<Cache>,
}

/// A lower-hex SHA-256 as bytes (`None` for anything else).
pub(crate) fn hash_bytes(hex_hash: &str) -> Option<[u8; 32]> {
    let mut out = [0u8; 32];
    hex::decode_to_slice(hex_hash, &mut out).ok()?;
    Some(out)
}

impl Sessions {
    pub(crate) fn new(
        config: &Config,
        store: Store,
        clock: SharedClock,
        log: Logger,
        events: Arc<dyn SessionEvents>,
        cache_size: usize,
    ) -> Sessions {
        Sessions {
            store,
            clock,
            log,
            events,
            idle_ms: config.session_idle_days.saturating_mul(DAY_MS),
            max_ms: config.session_max_days.saturating_mul(DAY_MS),
            max_per_user: config.max_sessions_per_user,
            cache: Mutex::new(Cache { map: LruMap::new(cache_size), epoch: 0 }),
        }
    }

    fn now(&self) -> i64 {
        self.clock.wall_ms()
    }

    /// Opens a session for `user`; the oldest sessions beyond `MAX_SESSIONS_PER_USER` are revoked
    /// (decided in the same transaction, so that parallel logins cannot add up).
    pub(crate) async fn create(
        &self,
        user: &User,
        client_label: Option<&str>,
        ip: Option<&str>,
    ) -> AuthResult<NewSessionToken> {
        let t = self.now();
        let token = random_token(SESSION_PREFIX);
        let token_hash = sha256_hex(&token);
        let expires_at = t.saturating_add(self.max_ms);
        let session = NewSession {
            user_id: user.id,
            token_hash: token_hash.clone(),
            created_at: t,
            expires_at,
            idle_expires_at: Some(expires_at.min(t.saturating_add(self.idle_ms))),
            client_label: client_label.filter(|s| !s.is_empty()).map(str::to_owned),
            ip: ip.filter(|s| !s.is_empty()).map(str::to_owned),
        };
        let (user_id, max) = (user.id, self.max_per_user);
        let (session_id, revoked) = self
            .store
            .write(move |db| {
                let id = db.sessions().create(&session)?;
                let revoked = db.sessions().enforce_limit(user_id, max, t)?;
                Ok::<_, store::StoreError>((id, revoked))
            })
            .await?;
        let others: Vec<String> = revoked.into_iter().filter(|h| *h != token_hash).collect();
        if !others.is_empty() {
            self.broadcast(user.id, Some(others));
        }
        Ok(NewSessionToken { token, token_hash, expires_at, session_id })
    }

    /// Reads the session of `hash` (an entry to cache).
    async fn load(&self, hash: &str, t: i64) -> AuthResult<Entry> {
        let hash = hash.to_owned();
        let session = self
            .store
            .read(move |db| {
                let Some(row) = db.sessions().by_token_hash(&hash)? else { return Ok(None) };
                if row.revoked_at.is_some() {
                    return Ok(None);
                }
                let user = db.users().by_id(row.user_id)?;
                Ok::<_, store::StoreError>(user.filter(|u| u.status == UserStatus::Active).map(|u| Cached {
                    id: row.id,
                    user_id: row.user_id,
                    username: u.username,
                    email_verified: u.email_verified,
                    last_seen_at: row.last_seen_at,
                    expires_at: row.expires_at,
                    idle_expires_at: row.idle_expires_at,
                }))
            })
            .await?;
        Ok(Entry { loaded_at: t, session })
    }

    /// The cached entry of `hash` when it is fresh at `t`.
    fn cached(&self, hash: &str, t: i64) -> Option<Entry> {
        let mut cache = self.cache.lock();
        let e = cache.map.get(hash)?;
        (t - e.loaded_at < SESSION_CACHE_TTL_MS && t >= e.loaded_at).then(|| e.clone())
    }

    /// Reads the session of `hash` and caches it, unless an invalidation landed during the read:
    /// the read is then made again, so that it sees the revocation that the invalidation follows.
    async fn load_and_cache(&self, hash: &str, t: i64) -> AuthResult<Entry> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let epoch = self.cache.lock().epoch;
            let entry = self.load(hash, t).await?;
            let mut cache = self.cache.lock();
            if cache.epoch == epoch {
                cache.map.insert(hash, entry.clone());
                return Ok(entry);
            }
            if attempt >= LOAD_ATTEMPTS {
                // Invalidations keep landing: answer from this read without caching it.
                return Ok(entry);
            }
        }
    }

    /// Validates a session token: the session and its account, or `None` (malformed, unknown,
    /// revoked, expired, or the account is no longer active).
    pub(crate) async fn validate(&self, token: &str) -> AuthResult<Option<SessionInfo>> {
        if !is_prefixed_token(token, SESSION_PREFIX) {
            return Ok(None);
        }
        let hash = sha256_hex(token);
        let t = self.now();
        let entry = match self.cached(&hash, t) {
            Some(e) => e,
            None => self.load_and_cache(&hash, t).await?,
        };
        let Some(s) = entry.session else { return Ok(None) };
        if s.expires_at <= t || s.idle_expires_at <= t {
            return Ok(None);
        }
        if t - s.last_seen_at >= SESSION_TOUCH_EVERY_MS {
            let idle = s.expires_at.min(t.saturating_add(self.idle_ms));
            match self.store.sessions().touch(s.id, t, idle).await {
                Ok(()) => {
                    if let Some(e) = self.cache.lock().map.peek_mut(&hash)
                        && let Some(c) = e.session.as_mut().filter(|c| c.id == s.id)
                    {
                        c.last_seen_at = t;
                        c.idle_expires_at = idle;
                    }
                }
                Err(e) => log_warn!(self.log, "session touch failed", { "err": log::error(&e) }),
            }
        }
        let token_hash = hash_bytes(&hash).expect("sha256_hex gives 64 hex digits");
        Ok(Some(SessionInfo {
            user_id: s.user_id,
            username: s.username,
            session_id: s.id,
            email_verified: s.email_verified,
            token_hash,
        }))
    }

    /// Drops cached sessions: those of `hashes` when there are any, else every session of
    /// `user_id`.
    pub(crate) fn invalidate(&self, user_id: Option<UserId>, hashes: &[String]) {
        let mut cache = self.cache.lock();
        cache.epoch = cache.epoch.wrapping_add(1);
        if !hashes.is_empty() {
            for h in hashes {
                cache.map.remove(h);
            }
        } else if let Some(user) = user_id {
            cache.map.remove_where(|e| e.session.as_ref().is_some_and(|s| s.user_id == user));
        }
    }

    /// Drops the cached sessions and tells the realtime layer: `Some(list)` closes the connections
    /// of the listed sessions (an empty list closes nothing: a refresh), `None` closes the user's
    /// connection.
    fn broadcast(&self, user_id: UserId, hashes: Option<Vec<String>>) {
        match hashes {
            None => {
                self.invalidate(Some(user_id), &[]);
                self.events.sessions_revoked(user_id, None);
            }
            Some(list) => {
                self.invalidate(Some(user_id), &list);
                if !list.is_empty() {
                    let bytes = list.iter().filter_map(|h| hash_bytes(h)).collect();
                    self.events.sessions_revoked(user_id, Some(bytes));
                }
            }
        }
    }

    /// Revokes one session of `user_id`. The hash the store gives back (else `token_hash`) closes
    /// that session's connection; without either, the user's cached sessions are read again.
    pub(crate) async fn revoke(
        &self,
        user_id: UserId,
        session_id: i64,
        token_hash: Option<&str>,
    ) -> AuthResult<()> {
        let h = self.store.sessions().revoke(session_id, Some(user_id), self.now()).await?;
        match h.or_else(|| token_hash.map(str::to_owned)) {
            Some(hex) => self.broadcast(user_id, Some(vec![hex])),
            None => self.refresh(user_id),
        }
        Ok(())
    }

    /// Revokes every session of `user_id` but `except`; returns the number revoked. Without
    /// `except` the user's connection is closed too.
    pub(crate) async fn revoke_all(&self, user_id: UserId, except: Option<i64>) -> AuthResult<usize> {
        let hashes = self.store.sessions().revoke_all_for_user(user_id, except, self.now()).await?;
        let n = hashes.len();
        self.broadcast(user_id, except.map(|_| hashes));
        Ok(n)
    }

    /// Closes the user's connection and drops the cached sessions after the store already
    /// deleted them (account anonymisation).
    pub(crate) fn revoked_elsewhere(&self, user_id: UserId) {
        self.broadcast(user_id, None);
    }

    /// Drops the cached sessions of `user_id` after an account change that the cache holds or
    /// that must be read again: nothing is revoked, the next request reloads its session.
    pub(crate) fn refresh(&self, user_id: UserId) {
        self.broadcast(user_id, Some(Vec::new()));
    }

    /// The active sessions of a user, most recently used first (`current`: the session of the
    /// request): `[{id, createdAt, lastSeenAt, expiresAt, clientLabel, current}]`.
    pub(crate) async fn list(&self, user_id: UserId, current: Option<i64>) -> AuthResult<Vec<Value>> {
        let t = self.now();
        let mut rows: Vec<store::SessionInfo> = self
            .store
            .sessions()
            .list_for_user(user_id)
            .await?
            .into_iter()
            .filter(|r| is_active(r, t))
            .collect();
        rows.sort_by_key(|r| std::cmp::Reverse(r.last_seen_at));
        Ok(rows
            .into_iter()
            .map(|r| {
                json!({
                    "id": r.id,
                    "createdAt": r.created_at,
                    "lastSeenAt": r.last_seen_at,
                    "expiresAt": r.expires_at,
                    "clientLabel": r.client_label,
                    "current": Some(r.id) == current,
                })
            })
            .collect())
    }

    /// The active session `id` of `user_id` (`id` as written in the request path: `"03"` is not
    /// session 3).
    pub(crate) async fn find(&self, user_id: UserId, id: &str) -> AuthResult<Option<store::SessionInfo>> {
        let t = self.now();
        Ok(self
            .store
            .sessions()
            .list_for_user(user_id)
            .await?
            .into_iter()
            .find(|r| r.id.to_string() == id && is_active(r, t)))
    }

    /// Sessions in the cache.
    pub(crate) fn cache_len(&self) -> usize {
        self.cache.lock().map.len()
    }
}

fn is_active(r: &store::SessionInfo, t: i64) -> bool {
    r.revoked_at.is_none() && r.expires_at > t && r.idle_expires_at > t
}
