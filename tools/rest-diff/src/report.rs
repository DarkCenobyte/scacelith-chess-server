//! The record of every compared step and the final report: open differences, differences
//! accepted by `accepted.txt` (documented intended deviations), harness warnings and the
//! endpoint coverage.

use std::fmt::Write as _;

use crate::diff::Difference;

/// One accepted deviation: steps matching `step` (a glob over `profile/scenario/step`) whose
/// difference aspect starts with `aspect` are listed as accepted with `reason`.
#[derive(Clone, Debug)]
pub struct AcceptRule {
    /// Glob over the step id (`*` matches any run of characters).
    pub step: String,
    /// Prefix of the difference aspect.
    pub aspect: String,
    /// Why it is accepted.
    pub reason: String,
}

/// Reads `accepted.txt`: `step glob | aspect prefix | reason` per line, `#` comments.
pub fn parse_accepted(text: &str) -> Result<Vec<AcceptRule>, String> {
    let mut rules = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let parts: Vec<&str> = line.splitn(3, '|').map(str::trim).collect();
        if parts.len() != 3 || parts.iter().any(|p| p.is_empty()) {
            return Err(format!("accepted.txt line {}: expected `step | aspect | reason`", n + 1));
        }
        rules.push(AcceptRule { step: parts[0].into(), aspect: parts[1].into(), reason: parts[2].into() });
    }
    Ok(rules)
}

/// Glob match with `*` only.
pub fn glob(pattern: &str, s: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == s;
    }
    let mut rest = s;
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            match rest.strip_prefix(part) {
                Some(r) => rest = r,
                None => return false,
            }
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            match rest.find(part) {
                Some(p) => rest = &rest[p + part.len()..],
                None => return false,
            }
        }
    }
    true
}

/// A compared step.
#[derive(Clone, Debug)]
pub struct StepRecord {
    /// `profile/scenario/step`.
    pub id: String,
    /// The request (Node side) in one line.
    pub request: String,
    /// Statuses (node, rust).
    pub statuses: (u16, u16),
    /// Differences, each with the reason it is accepted, if it is.
    pub diffs: Vec<(Difference, Option<String>)>,
}

/// Everything recorded during a run.
#[derive(Debug, Default)]
pub struct Report {
    /// Accepted deviations.
    pub accepted: Vec<AcceptRule>,
    /// Compared steps.
    pub steps: Vec<StepRecord>,
    /// Harness warnings (a status both servers gave but the scenario did not expect...).
    pub warnings: Vec<String>,
    /// Limits measured by the bursts: requests let through before the first refusal.
    pub limits: Vec<String>,
    /// Endpoints requested (method and path template).
    pub endpoints: Vec<String>,
    /// Profiles run.
    pub profiles: Vec<String>,
}

/// The endpoints of docs/API.md section 2 (and the health and page routes), as
/// `METHOD template` with `:param` segments.
pub const API_ENDPOINTS: &[&str] = &[
    "GET /api/v1/info",
    "POST /api/v1/auth/register",
    "POST /api/v1/auth/login",
    "POST /api/v1/auth/login/mfa",
    "POST /api/v1/auth/logout",
    "POST /api/v1/auth/logout-all",
    "POST /api/v1/auth/verify-email/resend",
    "POST /api/v1/auth/password/forgot",
    "POST /api/v1/auth/password/reset",
    "POST /api/v1/auth/sso/google/start",
    "POST /api/v1/auth/sso/google/finish",
    "POST /api/v1/auth/sso/google/link",
    "POST /api/v1/auth/sso/complete",
    "GET /api/v1/auth/sessions",
    "DELETE /api/v1/auth/sessions/:id",
    "GET /api/v1/account/me",
    "PUT /api/v1/account/preferences",
    "POST /api/v1/account/password",
    "POST /api/v1/account/mfa/totp/setup",
    "POST /api/v1/account/mfa/totp/enable",
    "POST /api/v1/account/mfa/totp/disable",
    "POST /api/v1/account/mfa/recovery-codes",
    "POST /api/v1/account/email",
    "POST /api/v1/account/export",
    "POST /api/v1/account/delete",
    "GET /api/v1/account/games",
    "GET /api/v1/games/:id",
    "GET /api/v1/games/:id/pgn",
    "GET /api/v1/games/:id/gif",
    "POST /api/v1/gif",
    "GET /api/v1/players/:username",
    "GET /api/v1/players/:username/games",
    "GET /api/v1/leaderboard",
    "POST /api/v1/reports",
    "GET /verify-email",
    "POST /verify-email",
    "GET /reset-password",
    "POST /reset-password",
    "GET /confirm-email-change",
    "POST /confirm-email-change",
    "GET /healthz",
    "GET /readyz",
    "GET /api/v1/healthz",
    "GET /api/v1/readyz",
];

/// The `API_ENDPOINTS` entry a request falls under, if any.
pub fn endpoint_of(method: &str, target: &str) -> Option<&'static str> {
    let path = target.split('?').next().unwrap_or_default();
    let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
    let method = if method == "HEAD" { "GET" } else { method };
    let segs: Vec<&str> = path.split('/').collect();
    let mut best: Option<(&'static str, usize)> = None;
    for e in API_ENDPOINTS {
        let (m, t) = e.split_once(' ').expect("METHOD template");
        if m != method {
            continue;
        }
        let tsegs: Vec<&str> = t.split('/').collect();
        if tsegs.len() != segs.len() {
            continue;
        }
        let mut literal = 0;
        let ok = tsegs.iter().zip(&segs).all(|(ts, s)| {
            if ts.starts_with(':') {
                !s.is_empty()
            } else {
                literal += 1;
                ts == s
            }
        });
        if ok && best.is_none_or(|(_, l)| literal > l) {
            best = Some((e, literal));
        }
    }
    best.map(|(e, _)| e)
}

impl Report {
    /// Records a compared step, marking the accepted differences.
    pub fn record(&mut self, id: String, request: String, statuses: (u16, u16), diffs: Vec<Difference>) {
        let diffs = diffs
            .into_iter()
            .map(|d| {
                let reason = self
                    .accepted
                    .iter()
                    .find(|r| glob(&r.step, &id) && (r.aspect == "*" || d.aspect.starts_with(&r.aspect)))
                    .map(|r| r.reason.clone());
                (d, reason)
            })
            .collect();
        self.steps.push(StepRecord { id, request, statuses, diffs });
    }

    /// Notes an endpoint as covered.
    pub fn cover(&mut self, method: &str, target: &str) {
        if let Some(e) = endpoint_of(method, target)
            && !self.endpoints.iter().any(|x| x == e)
        {
            self.endpoints.push(e.to_string());
        }
    }

    /// Number of open (not accepted) differences.
    pub fn open_count(&self) -> usize {
        self.steps.iter().map(|s| s.diffs.iter().filter(|(_, r)| r.is_none()).count()).sum()
    }

    /// The whole report as text.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let with_diff = self.steps.iter().filter(|s| !s.diffs.is_empty()).count();
        let open_steps = self.steps.iter().filter(|s| s.diffs.iter().any(|(_, r)| r.is_none())).count();
        let accepted: usize =
            self.steps.iter().map(|s| s.diffs.iter().filter(|(_, r)| r.is_some()).count()).sum();
        let _ = writeln!(out, "rest-diff: Node.js server (reference) vs Rust server");
        let _ = writeln!(out, "profiles: {}", self.profiles.join(", "));
        let _ = writeln!(
            out,
            "steps compared: {}, identical: {}, with differences: {} ({} with open differences)",
            self.steps.len(),
            self.steps.len() - with_diff,
            with_diff,
            open_steps
        );
        let _ = writeln!(out, "differences: {} open, {} accepted", self.open_count(), accepted);
        let _ = writeln!(out, "\n== Open differences ==");
        let mut any = false;
        for s in &self.steps {
            let open: Vec<&Difference> =
                s.diffs.iter().filter(|(_, r)| r.is_none()).map(|(d, _)| d).collect();
            if open.is_empty() {
                continue;
            }
            any = true;
            let _ =
                writeln!(out, "\n[{}] {}  (node {} / rust {})", s.id, s.request, s.statuses.0, s.statuses.1);
            for d in open {
                let _ = writeln!(out, "  {}:\n    node: {}\n    rust: {}", d.aspect, d.node, d.rust);
            }
        }
        if !any {
            let _ = writeln!(out, "(none)");
        }
        let _ = writeln!(out, "\n== Accepted differences ==");
        let mut by_reason: Vec<(String, Vec<String>)> = Vec::new();
        for s in &self.steps {
            for (d, r) in &s.diffs {
                if let Some(reason) = r {
                    let line = format!("[{}] {}: node {} | rust {}", s.id, d.aspect, d.node, d.rust);
                    match by_reason.iter_mut().find(|(x, _)| x == reason) {
                        Some((_, v)) => v.push(line),
                        None => by_reason.push((reason.clone(), vec![line])),
                    }
                }
            }
        }
        if by_reason.is_empty() {
            let _ = writeln!(out, "(none)");
        }
        for (reason, lines) in &by_reason {
            let _ = writeln!(out, "\n* {reason} ({} differences)", lines.len());
            for l in lines.iter().take(6) {
                let _ = writeln!(out, "    {}", crate::diff::clip(l));
            }
            if lines.len() > 6 {
                let _ = writeln!(out, "    ... and {} more", lines.len() - 6);
            }
        }
        if !self.limits.is_empty() {
            let _ = writeln!(out, "\n== Measured limits (requests let through before the first 429) ==");
            for l in &self.limits {
                let _ = writeln!(out, "  {l}");
            }
        }
        let _ = writeln!(out, "\n== Harness warnings ==");
        if self.warnings.is_empty() {
            let _ = writeln!(out, "(none)");
        }
        for w in &self.warnings {
            let _ = writeln!(out, "  {w}");
        }
        let _ = writeln!(out, "\n== Endpoint coverage ==");
        for e in API_ENDPOINTS {
            let hit = self.endpoints.iter().any(|x| x == e);
            let steps = self
                .steps
                .iter()
                .filter(|s| {
                    let (m, t) = s.request.split_once(' ').unwrap_or_default();
                    endpoint_of(m, t.split(' ').next().unwrap_or_default()) == Some(*e)
                })
                .count();
            let _ = writeln!(out, "  {} {e} ({steps} steps)", if hit { "[x]" } else { "[ ]" });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs_and_endpoints() {
        assert!(glob("*/info/*", "main/info/get"));
        assert!(glob("main/*", "main/x/y"));
        assert!(!glob("main/*/z", "main/x/y"));
        assert!(glob("a", "a"));
        assert_eq!(endpoint_of("GET", "/api/v1/games/123/pgn?x=1"), Some("GET /api/v1/games/:id/pgn"));
        assert_eq!(endpoint_of("HEAD", "/api/v1/info/"), Some("GET /api/v1/info"));
        assert_eq!(endpoint_of("POST", "/api/v1/auth/login/mfa"), Some("POST /api/v1/auth/login/mfa"));
        assert_eq!(endpoint_of("GET", "/api/v1/players/alice"), Some("GET /api/v1/players/:username"));
        assert_eq!(endpoint_of("GET", "/nope"), None);
        let rules = parse_accepted("# c\nmain/* | status | why\n").unwrap();
        assert_eq!(rules.len(), 1);
        assert!(parse_accepted("x | y").is_err());
    }
}
