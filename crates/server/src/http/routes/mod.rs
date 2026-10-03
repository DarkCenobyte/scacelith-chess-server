//! The `/api/v1` endpoints (DESIGN 5.9, docs/API.md). Owners: auth (auth, account, MFA, SSO,
//! export) and routes (info, players, leaderboard, games, account games, reports, GIF), wave 2.

pub mod account;
pub mod account_export;
pub mod auth;
pub mod sso;

use super::router::Router;

/// The services of the endpoints of the auth owner (auth, account, SSO, export).
#[derive(Clone)]
pub struct AuthGroups {
    /// `/auth/*`.
    pub auth: auth::AuthRouteDeps,
    /// `/account/*`.
    pub account: account::AccountRouteDeps,
    /// `/auth/sso/*`.
    pub sso: sso::SsoRouteDeps,
    /// `/account/export`.
    pub export: account_export::ExportRouteDeps,
}

/// Registers every API endpoint, in the order of the Node server's route modules (info, auth,
/// account, sso, players, reports, account-games, account-export, gif): the `Allow` lists follow
/// it. Route modules add the services they need to this signature.
pub fn register(router: &mut Router, auth_groups: AuthGroups) {
    // info (routes)
    auth::register(router, auth_groups.auth);
    account::register(router, auth_groups.account);
    sso::register(router, auth_groups.sso);
    // players, reports, account-games (routes)
    account_export::register(router, auth_groups.export);
    // gif (routes)
}
