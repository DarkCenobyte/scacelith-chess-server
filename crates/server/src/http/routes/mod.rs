//! The `/api/v1` endpoints (DESIGN 5.9, docs/API.md). Owners: auth (auth, account, MFA, SSO,
//! export) and routes (info, players, leaderboard, games, account games, reports, GIF), wave 2.

use super::router::Router;

/// Registers every API endpoint, in the order of the Node server's route modules (info, auth,
/// account, sso, players, reports, account-games, account-export, gif): the `Allow` lists follow
/// it. Route modules add the services they need to this signature.
pub fn register(router: &mut Router) {
    let _ = router;
}
