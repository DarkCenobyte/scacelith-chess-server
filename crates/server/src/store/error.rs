//! Errors of the store.

use std::fmt;

use rusqlite::ffi;

use crate::ids::GameId;

/// What went wrong, as a stable category. [`ErrorKind::code`] gives the snake_case identifier the
/// Node store used (`busy`, `username_taken`...), which the routes and logs still use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorKind {
    /// The database lock could not be taken within `busy_timeout` (SQLITE_BUSY / SQLITE_LOCKED).
    Busy,
    /// A unique constraint on `username_lower` (users or pending signups).
    UsernameTaken,
    /// A unique constraint on `email_normalized` (users or pending signups).
    EmailTaken,
    /// The SSO identity is already linked to another account.
    SsoTaken,
    /// Any other unique or primary key constraint.
    Duplicate,
    /// A foreign key constraint (an unknown user or game).
    ForeignKey,
    /// An invalid argument or a CHECK / NOT NULL constraint.
    Invalid,
    /// A finished-game record that cannot be stored (see [`StoreError::game_id`]).
    InvalidRecord,
    /// The row the operation needs does not exist.
    NotFound,
    /// A write on a read-only store or connection.
    ReadOnly,
    /// A rated game was committed by a store opened without a rating function.
    NoRatingFunction,
    /// An applied migration's file changed since it was applied.
    MigrationChecksum,
    /// The database has a migration this server does not know (a newer server wrote it).
    MigrationMissing,
    /// A migration failed and was rolled back.
    MigrationFailed,
    /// The store is closed, or closed before answering this request (its outcome is unknown).
    Closed,
    /// Any other SQLite or I/O failure.
    Sqlite,
    /// A job of the store panicked (its transaction was rolled back).
    Internal,
}

impl ErrorKind {
    /// The snake_case code of the kind.
    pub fn code(self) -> &'static str {
        match self {
            ErrorKind::Busy => "busy",
            ErrorKind::UsernameTaken => "username_taken",
            ErrorKind::EmailTaken => "email_taken",
            ErrorKind::SsoTaken => "sso_taken",
            ErrorKind::Duplicate => "duplicate",
            ErrorKind::ForeignKey => "foreign_key",
            ErrorKind::Invalid => "invalid",
            ErrorKind::InvalidRecord => "invalid_record",
            ErrorKind::NotFound => "not_found",
            ErrorKind::ReadOnly => "readonly",
            ErrorKind::NoRatingFunction => "no_rating_function",
            ErrorKind::MigrationChecksum => "migration_checksum",
            ErrorKind::MigrationMissing => "migration_missing",
            ErrorKind::MigrationFailed => "migration_failed",
            ErrorKind::Closed => "closed",
            ErrorKind::Sqlite => "sqlite",
            ErrorKind::Internal => "internal",
        }
    }

    /// Whether the kind comes from a constraint of the schema (unique, foreign key, check).
    pub fn is_constraint(self) -> bool {
        matches!(
            self,
            ErrorKind::UsernameTaken
                | ErrorKind::EmailTaken
                | ErrorKind::Duplicate
                | ErrorKind::ForeignKey
                | ErrorKind::Invalid
        )
    }
}

/// An error of the store: a kind, a message and, for a failed game commit, the game at fault.
#[derive(Debug)]
pub struct StoreError {
    kind: ErrorKind,
    message: String,
    game_id: Option<GameId>,
    source: Option<Box<rusqlite::Error>>,
}

/// Message of the requests a closing writer did not answer in time.
pub(crate) const UNANSWERED: &str = "store writer closed before answering (outcome unknown)";

impl StoreError {
    /// An error of the given kind.
    pub fn new(kind: ErrorKind, message: impl Into<String>) -> StoreError {
        StoreError { kind, message: message.into(), game_id: None, source: None }
    }

    /// An [`ErrorKind::Invalid`] error.
    pub fn invalid(message: impl Into<String>) -> StoreError {
        StoreError::new(ErrorKind::Invalid, message)
    }

    /// An [`ErrorKind::NotFound`] error.
    pub fn not_found(message: impl Into<String>) -> StoreError {
        StoreError::new(ErrorKind::NotFound, message)
    }

    /// The error of a request sent to a closed store.
    pub fn closed() -> StoreError {
        StoreError::new(ErrorKind::Closed, "store writer closed")
    }

    /// The error of a request the writer did not answer before its close timed out: it may have
    /// been carried out or not.
    pub fn unanswered() -> StoreError {
        StoreError::new(ErrorKind::Closed, UNANSWERED)
    }

    /// The error of a job that panicked.
    pub(crate) fn panicked() -> StoreError {
        StoreError::new(ErrorKind::Internal, "store job panicked; its transaction was rolled back")
    }

    /// Attaches the game whose record caused the error (finished-game commits).
    pub fn with_game(mut self, game_id: GameId) -> StoreError {
        self.game_id = Some(game_id);
        self
    }

    /// The kind of the error.
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The snake_case code of the kind.
    pub fn code(&self) -> &'static str {
        self.kind.code()
    }

    /// The message (SQLite's text for constraint failures).
    pub fn message(&self) -> &str {
        &self.message
    }

    /// The game whose record failed a commit batch: retrying the batch without it can succeed.
    pub fn game_id(&self) -> Option<GameId> {
        self.game_id
    }

    /// Refines a unique-constraint error into `username_taken` / `email_taken` from the column
    /// named in SQLite's message (users and pending signups).
    pub(crate) fn user_clash(self) -> StoreError {
        if self.kind != ErrorKind::Duplicate {
            return self;
        }
        let kind = if self.message.contains("username_lower") {
            ErrorKind::UsernameTaken
        } else if self.message.contains("email_normalized") {
            ErrorKind::EmailTaken
        } else {
            return self;
        };
        StoreError { kind, ..self }
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.source.as_deref().map(|e| e as &(dyn std::error::Error + 'static))
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> StoreError {
        let (kind, message) = match &e {
            rusqlite::Error::SqliteFailure(f, msg) => {
                let text = msg.clone().unwrap_or_else(|| f.to_string());
                (kind_of(f.extended_code), text)
            }
            other => (ErrorKind::Sqlite, other.to_string()),
        };
        if kind == ErrorKind::Busy {
            super::metrics::BUSY.inc();
        }
        StoreError { kind, message, game_id: None, source: Some(Box::new(e)) }
    }
}

/// The kind of an extended SQLite result code.
fn kind_of(extended: i32) -> ErrorKind {
    match extended & 0xff {
        ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => ErrorKind::Busy,
        ffi::SQLITE_READONLY => ErrorKind::ReadOnly,
        ffi::SQLITE_CONSTRAINT => match extended {
            ffi::SQLITE_CONSTRAINT_UNIQUE | ffi::SQLITE_CONSTRAINT_PRIMARYKEY => ErrorKind::Duplicate,
            ffi::SQLITE_CONSTRAINT_FOREIGNKEY => ErrorKind::ForeignKey,
            _ => ErrorKind::Invalid,
        },
        _ => ErrorKind::Sqlite,
    }
}

/// Result of the store's operations.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_codes_map_to_kinds() {
        assert_eq!(kind_of(ffi::SQLITE_BUSY), ErrorKind::Busy);
        assert_eq!(kind_of(ffi::SQLITE_LOCKED_SHAREDCACHE), ErrorKind::Busy);
        assert_eq!(kind_of(ffi::SQLITE_BUSY_SNAPSHOT), ErrorKind::Busy);
        assert_eq!(kind_of(ffi::SQLITE_CONSTRAINT_UNIQUE), ErrorKind::Duplicate);
        assert_eq!(kind_of(ffi::SQLITE_CONSTRAINT_PRIMARYKEY), ErrorKind::Duplicate);
        assert_eq!(kind_of(ffi::SQLITE_CONSTRAINT_FOREIGNKEY), ErrorKind::ForeignKey);
        assert_eq!(kind_of(ffi::SQLITE_CONSTRAINT_CHECK), ErrorKind::Invalid);
        assert_eq!(kind_of(ffi::SQLITE_CONSTRAINT_NOTNULL), ErrorKind::Invalid);
        assert_eq!(kind_of(ffi::SQLITE_READONLY), ErrorKind::ReadOnly);
        assert_eq!(kind_of(ffi::SQLITE_IOERR), ErrorKind::Sqlite);
    }

    #[test]
    fn unique_clashes_are_refined_by_column() {
        let e = StoreError::new(ErrorKind::Duplicate, "UNIQUE constraint failed: users.username_lower");
        assert_eq!(e.user_clash().kind(), ErrorKind::UsernameTaken);
        let e = StoreError::new(
            ErrorKind::Duplicate,
            "UNIQUE constraint failed: pending_signups.email_normalized",
        );
        assert_eq!(e.user_clash().kind(), ErrorKind::EmailTaken);
        let e = StoreError::new(ErrorKind::Duplicate, "UNIQUE constraint failed: pending_signups.token_hash");
        assert_eq!(e.user_clash().kind(), ErrorKind::Duplicate);
        let e = StoreError::invalid("x").with_game(7);
        assert_eq!((e.code(), e.game_id()), ("invalid", Some(7)));
    }
}
