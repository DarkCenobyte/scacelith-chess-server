//! The whole server inside the calling process, for the tools of this workspace that must replace
//! what no setting may change: Google's OpenID endpoints, by a local fake provider
//! (`tools/live-check`, its Google sign-in part). Not a stable interface: the server binary does
//! not use it, and a production server always talks to Google.

use std::sync::Arc;

use crate::app::{Instance, LaunchOptions};
use crate::auth::OidcOptions;
use crate::config::Config;

/// A server started by [`start`], serving until [`Embedded::stop`].
#[derive(Debug)]
pub struct Embedded {
    instance: Instance,
}

/// Starts every service of `config` as `scacelith-server start` does (the caller sets up the
/// logger and handles the signals), with Google sign-in going to `oidc`, and announces it ready.
pub async fn start(config: Config, oidc: OidcOptions) -> Result<Embedded, String> {
    let options = LaunchOptions { oidc: Some(oidc), ..LaunchOptions::default() };
    let instance = Instance::launch(Arc::new(config), options).await.map_err(|e| e.to_string())?;
    instance.announce_ready();
    Ok(Embedded { instance })
}

impl Embedded {
    /// The graceful stop of a SIGTERM: the players are warned, the games journaled, the database
    /// closed.
    pub async fn stop(self) {
        self.instance.shutdown("SIGTERM").await;
    }
}
