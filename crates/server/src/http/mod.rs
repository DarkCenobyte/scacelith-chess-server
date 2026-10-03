//! HTTPS API framework and routes: router, answers and headers, JSON bodies and schema checks,
//! authentication, per-route and per-account rate limits, handler timeout, every `/api/v1`
//! endpoint and the HTML pages. Framework owner: net; routes: auth and routes (wave 2). See
//! docs/RUST-PORT.md.
//!
//! A route module registers its endpoints on a [`Router`]:
//!
//! ```ignore
//! router.get("/players/:username", RouteOpts::new().auth(AuthMode::Optional)
//!     .rate(RateSpec::new("public_read", 60.0, 60_000).by_user()), move |ctx: Ctx| async move {
//!         let name = ctx.param("username").unwrap_or_default();
//!         Ok(Answer::json(json!({ "username": name })))
//!     });
//! ```
//!
//! [`Api::builder`] turns the router into the pipeline that the listener serves.

pub mod answer;
pub mod api;
pub mod body;
pub mod ctx;
pub mod json;
pub mod pages;
pub mod rates;
pub mod router;
pub mod routes;
pub mod schema;
#[cfg(test)]
pub mod testing;
pub mod url;

#[cfg(test)]
mod tests;

pub use answer::{Answer, ApiError, ApiErrorData, Payload};
pub use api::{Api, ApiBuilder, PageRenderer};
pub use ctx::{AuthInfo, Authenticator, Ctx, NoAuthenticator};
pub use rates::RateSpec;
pub use router::{AuthMode, RouteOpts, Router};
pub use schema::{Schema, Spec};
