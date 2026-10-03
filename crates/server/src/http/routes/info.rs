//! `GET /api/v1/info`: what a game client needs to know before connecting (DESIGN 5.9,
//! docs/API.md; the Node server's `src/http/routes/info.js`). No session and no limit of its own
//! (only the per-address budget). The document is built once; the store's server id is read until
//! the store has one, then kept, so an answer costs no SQLite read.

use std::sync::Arc;

use arc_swap::ArcSwapOption;
use scacelith_protocol::{FINGERPRINT, PROTOCOL_VERSION, SUBPROTOCOL};
use serde_json::{Value, json};

use crate::config::Config;
use crate::http::{Answer, ApiError, AuthMode, RouteOpts, Router};
use crate::security::password::PASSWORD_MAX_BYTES;
use crate::store::Store;

/// The oldest protocol version the server speaks.
pub const PROTOCOL_MIN: u16 = 1;

/// The usernames a new account may take (the auth module's `USERNAME_PATTERN`), as its source.
pub const USERNAME_PATTERN: &str = "^[A-Za-z0-9][A-Za-z0-9_-]*$";

/// The services of the info route.
#[derive(Clone)]
pub struct InfoDeps {
    /// The configuration.
    pub config: Arc<Config>,
    /// The store (the server id).
    pub store: Store,
}

/// A number of seconds from milliseconds, integral when it is.
fn seconds(ms: i64) -> Value {
    if ms % 1000 == 0 { Value::from(ms / 1000) } else { Value::from(ms as f64 / 1000.0) }
}

/// The `/info` document without its server id (`serverId` is `null` in it).
pub fn info_document(config: &Config) -> Value {
    let categories: Vec<Value> = config
        .categories
        .iter()
        .map(|c| json!({ "id": c.id, "baseSec": seconds(c.base_ms), "incSec": seconds(c.inc_ms) }))
        .collect();
    json!({
        "name": config.server_name,
        "serverId": null,
        "motd": config.server_motd,
        "protocol": {
            "min": PROTOCOL_MIN,
            "max": PROTOCOL_VERSION,
            "schema": FINGERPRINT,
            "subprotocol": SUBPROTOCOL,
        },
        "wsPort": config.public_ws_port,
        "wsPath": "/ws",
        "registration": config.registration.as_str(),
        "emailVerification": config.require_email_verification,
        "sso": { "google": config.sso_google_enabled && !config.google_client_id.is_empty() },
        "mfa": true,
        "pow": { "register": config.pow_register_bits },
        "categories": categories,
        "limits": {
            "usernameMin": config.username_min,
            "usernameMax": config.username_max,
            "usernamePattern": USERNAME_PATTERN,
            "passwordMinLength": config.password_min_length,
            "passwordMaxBytes": PASSWORD_MAX_BYTES,
            "customTimeControls": config.allow_custom_time_controls,
            "reportsPerDay": config.reports_per_day,
            "wsMaxMessageBytes": scacelith_protocol::MAX_CLIENT_MESSAGE,
        },
    })
}

struct Info {
    store: Store,
    /// The document without the server id.
    base: Arc<Value>,
    /// The document with the server id, once the store had one.
    complete: ArcSwapOption<Value>,
}

impl Info {
    async fn document(&self) -> Arc<Value> {
        if let Some(doc) = self.complete.load_full() {
            return doc;
        }
        // An error or a store without an id answers `null`, and the id is read again next time.
        let Ok(Some(id)) = self.store.server_id().await else {
            return self.base.clone();
        };
        let mut doc = Value::clone(&self.base);
        if let Some(fields) = doc.as_object_mut() {
            fields.insert("serverId".into(), Value::String(id));
        }
        let doc = Arc::new(doc);
        self.complete.store(Some(doc.clone()));
        doc
    }
}

/// Registers `GET /info`.
pub fn register(router: &mut Router, deps: InfoDeps) {
    let info = Arc::new(Info {
        base: Arc::new(info_document(&deps.config)),
        store: deps.store,
        complete: ArcSwapOption::empty(),
    });
    router.get("/info", RouteOpts::new().auth(AuthMode::None), move |_ctx| {
        let info = info.clone();
        async move { Ok::<_, ApiError>(Answer::json(Value::clone(&*info.document().await))) }
    });
}

#[cfg(test)]
mod tests;
