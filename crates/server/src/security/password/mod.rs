//! Passwords (DESIGN.md section 8): the policy, Argon2id hashing, the bounded FIFO hash limiter
//! with its busy conditions, the per-request wait budget and the padding floor of failed login
//! checks.
//!
//! Typical use by the auth service:
//!
//! ```ignore
//! let hasher = LimitedHasher::new(
//!     Arc::new(Argon2Hasher::default()),
//!     HashLimiter::new(HashLimiterConfig::from_config(&config))?,
//!     Arc::new(CheckFloor::new(clock.clone())),
//!     log.clone(),
//! );
//! // At start-up, in the background:
//! //   if let Err(e) = hasher.warm_up().await { log_warm_up_failure(&log, &e) }
//! let budget = hasher.budget(Some(ip));
//! match hasher.check_password(stored, password, &budget.next()).await {
//!     Ok(v) if v.ok => { /* ... */ }
//!     Ok(_) => { /* 401 invalid_credentials (already padded) */ }
//!     Err(HashError::Busy(reason)) => match reason.answer() { /* 503 / 429 / skip */ },
//!     Err(HashError::Failed(e)) => { /* 500 */ }
//! }
//! ```

mod floor;
mod hasher;
mod limited;
mod limiter;
mod policy;

pub use floor::CheckFloor;
pub use hasher::{
    Argon2Hasher, Argon2Params, HashFailure, MAX_MEMORY_KIB, ParsedHash, PasswordHasher, Verified, parse_hash,
};
pub use limited::{HashBudget, HashError, LimitedHasher, log_warm_up_failure};
pub use limiter::{
    BusyAnswer, BusyReason, HashLimiter, HashLimiterConfig, HashOpts, HashPermit, LimiterConfigError,
    LimiterStats, PasswordBusy,
};
pub use policy::{
    PASSWORD_MAX_BYTES, PolicyReason, PolicyViolation, check_password_policy, common_passwords,
    is_common_password, normalize_password,
};
