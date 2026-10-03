//! The HTML pages of the e-mail links (`/verify-email`, `/reset-password`,
//! `/confirm-email-change`) and their layout. Owner: auth (wave 2).
//!
//! Pages register with [`Router::page`]. The layout's `renderMessage` also renders the errors
//! of page routes: hand it to [`crate::http::ApiBuilder::page_renderer`].

use super::router::Router;

/// Registers the pages. Page modules add the services they need to this signature.
pub fn register(router: &mut Router) {
    let _ = router;
}
