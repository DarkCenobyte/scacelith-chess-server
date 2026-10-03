//! The `/api/v1` endpoints (DESIGN 5.9, docs/API.md). Owners: auth (auth, account, MFA, SSO,
//! export) and routes (info, players, leaderboard, games, account games, reports, GIF), wave 2.

pub mod account;
pub mod account_export;
pub mod account_games;
pub mod auth;
pub mod games;
pub mod gif;
pub mod info;
pub mod leaderboard;
pub mod players;
#[cfg(test)]
mod read_support;
pub mod reports;
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

/// The services of every endpoint group.
pub struct RouteGroups {
    /// `GET /info`.
    pub info: info::InfoDeps,
    /// Auth, account, SSO and export.
    pub auth: AuthGroups,
    /// `/players/*`.
    pub players: players::PlayersDeps,
    /// `GET /games/:id`, `GET /games/:id/pgn`.
    pub games: games::GamesDeps,
    /// `GET /leaderboard`.
    pub leaderboard: leaderboard::LeaderboardDeps,
    /// `POST /reports`.
    pub reports: reports::ReportsDeps,
    /// `GET /account/games`.
    pub account_games: account_games::AccountGamesDeps,
    /// `GET /games/:id/gif`, `POST /gif`.
    pub gif: gif::GifDeps,
}

/// Registers every API endpoint, in the order of the Node server's route modules (info, auth,
/// account, sso, players, games, leaderboard, reports, account-games, account-export, gif): the
/// `Allow` lists follow it.
pub fn register(router: &mut Router, groups: RouteGroups) {
    info::register(router, groups.info);
    auth::register(router, groups.auth.auth);
    account::register(router, groups.auth.account);
    sso::register(router, groups.auth.sso);
    players::register(router, groups.players);
    games::register(router, groups.games);
    leaderboard::register(router, groups.leaderboard);
    reports::register(router, groups.reports);
    account_games::register(router, groups.account_games);
    account_export::register(router, groups.auth.export);
    gif::register(router, groups.gif);
}

/// Registers the endpoints of the auth owner alone, in the same relative order (the auth tests).
#[cfg(test)]
pub(crate) fn register_auth(router: &mut Router, groups: AuthGroups) {
    auth::register(router, groups.auth);
    account::register(router, groups.account);
    sso::register(router, groups.sso);
    account_export::register(router, groups.export);
}
