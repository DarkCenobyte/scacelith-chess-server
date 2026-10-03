//! The store rows as the `--json` output of the administration commands shows them: the field
//! names and order of the former server's store objects (secrets never included).

use serde_json::{Map, Value, json};

use crate::anticheat::num::json_num;
use crate::store::{Anomaly, CategoryRating, GivenRefund, Job, Refund, Report, Sanction, Signup, User};

/// An account without its secrets (password hash, MFA secrets).
pub fn public_user(u: &User) -> Value {
    json!({
        "id": u.id,
        "username": u.username,
        "email": u.email,
        "emailVerified": u.email_verified,
        "mfaEnabled": u.mfa_enabled,
        "status": u.status.as_str(),
        "createdAt": u.created_at,
        "lastLoginAt": u.last_login_at,
        "acceptChallenges": u.accept_challenges,
    })
}

/// A pending signup without its password hash and link.
pub fn signup(p: &Signup) -> Value {
    json!({
        "username": p.username,
        "email": p.email,
        "createdAt": p.created_at,
        "expiresAt": p.expires_at,
        "link": p.token_hash.is_some(),
    })
}

/// A rating record of one category.
pub fn rating(r: &CategoryRating) -> Value {
    let rec = &r.record;
    json!({
        "category": r.category,
        "rating": rec.rating,
        "games": rec.games,
        "wins": rec.wins,
        "draws": rec.draws,
        "losses": rec.losses,
        "peak": rec.peak,
        "reachedSenior": rec.reached_senior,
        "rated": rec.rated,
        "countedGames": rec.counted_games,
        "unratedGames": rec.unrated_games,
        "unratedOpponents": rec.unrated_opponents,
        "unratedHalfPoints": rec.unrated_half_points,
        "provisional": r.provisional,
        "updatedAt": r.updated_at,
    })
}

/// A sanction.
pub fn sanction(s: &Sanction) -> Value {
    json!({
        "id": s.id,
        "userId": s.user_id,
        "kind": s.kind.as_str(),
        "reason": s.reason,
        "source": s.source.as_str(),
        "gameId": s.game_id,
        "startsAt": s.starts_at,
        "endsAt": s.ends_at,
        "createdAt": s.created_at,
        "createdBy": s.created_by,
        "liftedAt": s.lifted_at,
        "liftedBy": s.lifted_by,
    })
}

/// A report.
pub fn report(r: &Report) -> Value {
    json!({
        "id": r.id,
        "reporterId": r.reporter_id,
        "reportedId": r.reported_id,
        "gameId": r.game_id,
        "category": r.category.as_str(),
        "weight": json_num(r.weight),
        "status": r.status.as_str(),
        "createdAt": r.created_at,
        "resolvedAt": r.resolved_at,
        "resolvedBy": r.resolved_by,
        "comment": r.comment,
        "reporterName": r.reporter_name,
        "reportedName": r.reported_name,
    })
}

/// An anomaly, its detail read back as structured data.
pub fn anomaly(a: &Anomaly) -> Value {
    json!({
        "id": a.id,
        "userId": a.user_id,
        "gameId": a.game_id,
        "kind": a.kind,
        "severity": a.severity.as_str(),
        "at": a.at,
        "detail": a.detail.as_ref().map(structured),
    })
}

/// A refund as listed.
pub fn refund(r: &Refund) -> Value {
    json!({
        "id": r.id,
        "gameId": r.game_id,
        "victimId": r.victim_id,
        "cheaterId": r.cheater_id,
        "category": r.category,
        "points": r.points,
        "createdAt": r.created_at,
        "sanctionId": r.sanction_id,
        "source": r.source.as_str(),
        "createdBy": r.created_by,
        "notifiedAt": r.notified_at,
        "victimName": r.victim_name,
        "cheaterName": r.cheater_name,
    })
}

/// A refund just given.
pub fn given(r: &GivenRefund) -> Value {
    json!({
        "id": r.id,
        "gameId": r.game_id,
        "victimId": r.victim_id,
        "category": r.category,
        "points": r.points,
        "endedAt": r.ended_at,
    })
}

/// An analysis job.
pub fn job(j: &Job) -> Value {
    json!({
        "status": j.status.as_str(),
        "priority": j.priority as i64,
        "attempts": j.attempts,
        "queuedAt": j.queued_at,
        "finishedAt": j.finished_at,
        "error": j.error,
    })
}

/// A value stored as JSON text read back (twice when it was encoded twice); anything else as it
/// is.
pub fn structured(v: &Value) -> Value {
    let mut x = v.clone();
    for _ in 0..2 {
        let Value::String(s) = &x else { break };
        match serde_json::from_str(s) {
            Ok(parsed) => x = parsed,
            Err(_) => return v.clone(),
        }
    }
    x
}

/// An object built field by field, in order.
pub fn object(fields: impl IntoIterator<Item = (&'static str, Value)>) -> Value {
    Value::Object(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect::<Map<String, Value>>())
}
