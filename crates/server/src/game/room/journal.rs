//! Journal records of a room: the payloads the room writes, and the replay that rebuilds it.
//!
//! The journal stores `{kind, game, at, payload}`; the payloads are opaque to it. All integers
//! are little-endian; times are integer epoch milliseconds (`i64`). Extra bytes at the end of a
//! payload are ignored (room of a later build); a payload shorter than its kind's size is an
//! error.
//!
//! | kind | payload |
//! |---|---|
//! | 1 created | `u8 format (1) \| u8 flags (1 rated, 2 autoPress) \| u64 id \| u32 baseMs \| u32 incMs \| i64 createdAt \| u64 rematchOf \| white \| black \| str8 category`, a player being `u32 userId \| u16 rating \| u8 provisional \| str8 name` |
//! | 2 move | 32 bytes: `u16 ply \| u16 move \| u8 flags \| u8 bits (1 draw offer made with the move, 2 the move declined the opponent's offer) \| u16 0 \| u32 spentMs \| u32 clockAfter \| u32 quotaAfter \| u32 gseq (of its MoveMade) \| i64 recvTime` |
//! | 3 event | 12 bytes: `u8 kind \| u8 color \| u8 0 \| u8 0 \| u32 gseqAfter \| u32 arg`; kinds: 1 draw offer, 2 draw declined (color = decliner), 3 disconnect (arg = grace), 4 reconnect, 5 desync |
//! | 3 recovered | 16 bytes: `u8 6 \| u8 2 \| u8 recFlags (1: the first reconnection of each player restarts its first-move timer) \| u8 0 \| u32 gseqAfter \| u32 grace (0: the configured one) \| u32 clock hold` |
//! | 3 checkpoint | 68 bytes, see `GameRoom::checkpoint_record`: in [`GameRoom::journal_state`], and when a clock held since a recovery starts without its player |
//! | 4 ended | 24 bytes: `u8 status \| u8 reason \| u8 culprit \| u8 0 \| u32 whiteMs \| u32 blackMs \| u32 gseq (of its GameEnd) \| i64 endedAt` |
//! | 6 snapshot | the records of [`GameRoom::journal_state`] in one record (compaction): `u8 format (1) \| u8 0 \| u16 count \| count × (u8 kind \| u32 length \| i64 at \| payload)`; a replay starts from the latest snapshot and ignores the records before it |
//!
//! The round-trip averages and the rematch window are not journaled.

use scacelith_protocol::{EndReason, GameStatus, PlayerInfo};

use super::{
    EventKind, GameResult, GameRoom, PlyRecord, REC_FIRST_MOVE_RESTART, RoomError, RoomSettings, RoomSpec,
};
use crate::game::rules::{Rules, Side};
use crate::journal::{Record, RecordKind};

/// One journal record of a game, as the room produces it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalRecord {
    /// Kind of the record.
    pub kind: RecordKind,
    /// Time of the record (epoch ms).
    pub at: i64,
    /// Payload.
    pub payload: Vec<u8>,
}

/// A journal record a room can be replayed from: the room's own [`JournalRecord`], or a
/// [`Record`] read back from the shard journal (whose times are the integers the host appended).
pub trait JournalEntry {
    /// Kind of the record.
    fn kind(&self) -> RecordKind;
    /// Time of the record (epoch ms).
    fn at_ms(&self) -> i64;
    /// Payload.
    fn payload(&self) -> &[u8];
}

impl JournalEntry for JournalRecord {
    fn kind(&self) -> RecordKind {
        self.kind
    }

    fn at_ms(&self) -> i64 {
        self.at
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

impl JournalEntry for Record {
    fn kind(&self) -> RecordKind {
        self.kind
    }

    fn at_ms(&self) -> i64 {
        // The host appends integer times; `as` saturates on anything else.
        self.at as i64
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }
}

/// A journal that cannot be replayed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplayError(String);

impl ReplayError {
    fn new(message: impl Into<String>) -> Self {
        ReplayError(message.into())
    }

    /// What is wrong.
    #[must_use]
    pub fn message(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "journal: {}", self.0)
    }
}

impl std::error::Error for ReplayError {}

const CREATED_FORMAT: u8 = 1;
const CREATED_RATED: u8 = 1;
const CREATED_AUTO_PRESS: u8 = 2;
pub(crate) const MOVE_REC_BYTES: usize = 32;
pub(crate) const EVENT_REC_BYTES: usize = 12;
pub(crate) const RECOVERED_REC_BYTES: usize = 16;
pub(crate) const ENDED_REC_BYTES: usize = 24;
pub(crate) const CHECKPOINT_BYTES: usize = 68;
const SNAPSHOT_FORMAT: u8 = 1;
const SNAPSHOT_HEADER: usize = 4;
const SNAPSHOT_REC_HEADER: usize = 13;

// Bits of the presence byte of a checkpoint.
const CP_WHITE_CONNECTED: u8 = 1;
const CP_BLACK_CONNECTED: u8 = 2;
const CP_CLOCK_HELD: u8 = 4;
const CP_WHITE_RECOVERY_AWAY: u8 = 8;
const CP_BLACK_RECOVERY_AWAY: u8 = 16;

/// A record borrowed from a journal or from a snapshot payload (`kind` is `None` for a kind
/// this build does not know, found inside a snapshot).
#[derive(Clone, Copy, Debug)]
struct RecordView<'a> {
    kind: Option<RecordKind>,
    at: i64,
    payload: &'a [u8],
}

/// Bounds-checked little-endian reader of a payload.
struct Reader<'a> {
    b: &'a [u8],
    o: usize,
    what: &'static str,
}

impl<'a> Reader<'a> {
    fn new(b: &'a [u8], what: &'static str) -> Self {
        Reader { b, o: 0, what }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], ReplayError> {
        let end = self.o.checked_add(n).filter(|&e| e <= self.b.len());
        let Some(end) = end else {
            return Err(ReplayError::new(format!("short {} record", self.what)));
        };
        let s = &self.b[self.o..end];
        self.o = end;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], ReplayError> {
        let mut a = [0; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    fn u8(&mut self) -> Result<u8, ReplayError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, ReplayError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, ReplayError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn i32(&mut self) -> Result<i32, ReplayError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, ReplayError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn i64(&mut self) -> Result<i64, ReplayError> {
        Ok(i64::from_le_bytes(self.array()?))
    }

    fn str8(&mut self) -> Result<String, ReplayError> {
        let n = usize::from(self.u8()?);
        let s = self.take(n)?;
        String::from_utf8(s.to_vec())
            .map_err(|_| ReplayError::new(format!("bad text in the {} record", self.what)))
    }
}

fn put_str8(b: &mut Vec<u8>, s: &str) {
    // The room keeps names and categories short (24 and 7 bytes).
    let s = &s.as_bytes()[..s.len().min(255)];
    b.push(s.len() as u8);
    b.extend_from_slice(s);
}

fn put_player(b: &mut Vec<u8>, p: &PlayerInfo) {
    b.extend_from_slice(&p.user_id.to_le_bytes());
    b.extend_from_slice(&p.rating.to_le_bytes());
    b.push(u8::from(p.provisional));
    put_str8(b, &p.name);
}

fn read_player(r: &mut Reader<'_>) -> Result<PlayerInfo, ReplayError> {
    Ok(PlayerInfo { user_id: r.u32()?, rating: r.u16()?, provisional: r.u8()? != 0, name: r.str8()? })
}

/// Clamps a value into `u32` for a record field.
fn u32_field(v: i64) -> u32 {
    u32::try_from(v.max(0)).unwrap_or(u32::MAX)
}

/// Writes the header of one record inside a snapshot payload.
fn put_snapshot_header(b: &mut Vec<u8>, kind: RecordKind, at: i64, len: usize) {
    b.push(kind.as_u8());
    b.extend_from_slice(&u32::try_from(len).unwrap_or(u32::MAX).to_le_bytes());
    b.extend_from_slice(&at.to_le_bytes());
}

/// Writes one whole record inside a snapshot payload.
fn put_snapshot_record(b: &mut Vec<u8>, rec: &JournalRecord) {
    put_snapshot_header(b, rec.kind, rec.at, rec.payload.len());
    b.extend_from_slice(&rec.payload);
}

fn decode_snapshot(payload: &[u8]) -> Result<Vec<RecordView<'_>>, ReplayError> {
    let mut r = Reader::new(payload, "snapshot");
    if payload.len() < SNAPSHOT_HEADER || r.u8()? != SNAPSHOT_FORMAT {
        return Err(ReplayError::new("bad snapshot record"));
    }
    r.u8()?;
    let n = usize::from(r.u16()?);
    let mut records = Vec::with_capacity(n);
    for _ in 0..n {
        let kind = RecordKind::from_u8(r.u8()?);
        let len = r.u32()? as usize;
        let at = r.i64()?;
        records.push(RecordView { kind, at, payload: r.take(len)? });
    }
    if r.o != payload.len() {
        return Err(ReplayError::new("bad snapshot record length"));
    }
    Ok(records)
}

impl GameRoom {
    /// The `created` record (the host appends it when it creates the game).
    #[must_use]
    pub fn created_record(&self) -> JournalRecord {
        let mut b = Vec::with_capacity(64);
        b.push(CREATED_FORMAT);
        b.push(
            if self.rated { CREATED_RATED } else { 0 } | if self.auto_press { CREATED_AUTO_PRESS } else { 0 },
        );
        b.extend_from_slice(&self.id.to_le_bytes());
        b.extend_from_slice(&self.base_ms.to_le_bytes());
        b.extend_from_slice(&self.inc_ms.to_le_bytes());
        b.extend_from_slice(&self.created_at.to_le_bytes());
        b.extend_from_slice(&self.rematch_of.to_le_bytes());
        put_player(&mut b, &self.players[0]);
        put_player(&mut b, &self.players[1]);
        put_str8(&mut b, &self.category);
        JournalRecord { kind: RecordKind::Created, at: self.created_at, payload: b }
    }

    /// The room as a compact list of journal records (created, moves, one checkpoint, ended):
    /// [`GameRoom::from_journal`] on them rebuilds an identical room.
    #[must_use]
    pub fn journal_state(&self) -> Vec<JournalRecord> {
        let mut recs = Vec::with_capacity(self.ply() + 3);
        recs.push(self.created_record());
        recs.extend((0..self.ply()).map(|i| self.move_record(i)));
        recs.push(self.checkpoint_record(self.clock.turn_start()));
        if self.is_over() {
            recs.push(self.ended_record());
        }
        recs
    }

    /// The room as one `snapshot` record (journal compaction): [`GameRoom::journal_state`] in a
    /// single payload, which a replay uses in place of every earlier record of the game. Taken
    /// between two outcomes, it includes every record the host appended for this game so far.
    #[must_use]
    pub fn journal_snapshot(&self, now: i64) -> JournalRecord {
        let created = self.created_record();
        let check = self.checkpoint_record(self.clock.turn_start());
        let ended = self.is_over().then(|| self.ended_record());
        let n = self.ply();
        let count = n + if ended.is_some() { 3 } else { 2 };
        let size = SNAPSHOT_HEADER
            + count * SNAPSHOT_REC_HEADER
            + created.payload.len()
            + n * MOVE_REC_BYTES
            + check.payload.len()
            + ended.as_ref().map_or(0, |e| e.payload.len());
        let mut b = Vec::with_capacity(size);
        b.push(SNAPSHOT_FORMAT);
        b.push(0);
        // At most MAX_PLIES + 3 records.
        b.extend_from_slice(&(count as u16).to_le_bytes());
        put_snapshot_record(&mut b, &created);
        for (i, p) in self.plies.iter().enumerate() {
            put_snapshot_header(&mut b, RecordKind::Move, p.at, MOVE_REC_BYTES);
            self.write_move(&mut b, i);
        }
        put_snapshot_record(&mut b, &check);
        if let Some(e) = &ended {
            put_snapshot_record(&mut b, e);
        }
        debug_assert_eq!(b.len(), size);
        JournalRecord { kind: RecordKind::Snapshot, at: now, payload: b }
    }

    /// Rebuilds a room from its journal records, in order. A replay starts from the latest
    /// `snapshot` record when there is one (the records before it are ignored). With
    /// `strict == false`, the replay stops at the first bad record after the `created` one and
    /// keeps the room as it was ([`GameRoom::replay_error`] tells why) instead of failing.
    ///
    /// # Errors
    ///
    /// [`RoomError::Replay`] when the records cannot be replayed (no `created` record, a bad
    /// record when strict...), [`RoomError::InvalidGameId`] for a bad id in the `created` record.
    pub fn from_journal<R: JournalEntry>(
        records: &[R],
        settings: RoomSettings,
        rules: Box<dyn Rules>,
        strict: bool,
    ) -> Result<GameRoom, RoomError> {
        let views: Vec<RecordView<'_>> = records
            .iter()
            .map(|r| RecordView { kind: Some(r.kind()), at: r.at_ms(), payload: r.payload() })
            .collect();
        let views = match views.iter().rposition(|r| r.kind == Some(RecordKind::Snapshot)) {
            Some(base) => {
                let mut v = decode_snapshot(views[base].payload)?;
                v.extend_from_slice(&views[base + 1..]);
                v
            }
            None => views,
        };
        let Some(first) = views.first() else {
            return Err(ReplayError::new("no record").into());
        };
        if first.kind != Some(RecordKind::Created) {
            return Err(ReplayError::new("the first record is not `created`").into());
        }
        let mut room = GameRoom::new(Self::read_created(first.payload)?, settings, rules)?;
        for rec in &views[1..] {
            let applied = match rec.kind {
                Some(RecordKind::Move) => room.replay_move(rec.payload),
                Some(RecordKind::Event) => room.replay_event(rec.payload, rec.at),
                Some(RecordKind::Ended) => room.replay_ended(rec.payload),
                Some(RecordKind::Created) => Err(ReplayError::new("second created record")),
                Some(RecordKind::Snapshot) => Err(ReplayError::new("snapshot inside a snapshot")),
                // `committed` and unknown kinds carry no room state.
                Some(RecordKind::Committed) | None => Ok(()),
            };
            if let Err(e) = applied {
                if strict {
                    return Err(e.into());
                }
                room.replay_error = Some(e);
                break;
            }
        }
        Ok(room)
    }

    fn read_created(payload: &[u8]) -> Result<RoomSpec, ReplayError> {
        let mut r = Reader::new(payload, "created");
        if r.u8()? != CREATED_FORMAT {
            return Err(ReplayError::new("bad created record format"));
        }
        let flags = r.u8()?;
        Ok(RoomSpec {
            id: r.u64()?,
            base_ms: r.u32()?,
            inc_ms: r.u32()?,
            created_at: r.i64()?,
            rematch_of: r.u64()?,
            white: read_player(&mut r)?,
            black: read_player(&mut r)?,
            category: r.str8()?,
            rated: flags & CREATED_RATED != 0,
            auto_press: flags & CREATED_AUTO_PRESS != 0,
        })
    }

    /// The move record of ply `i`.
    pub(crate) fn move_record(&self, i: usize) -> JournalRecord {
        let mut b = Vec::with_capacity(MOVE_REC_BYTES);
        self.write_move(&mut b, i);
        JournalRecord { kind: RecordKind::Move, at: self.plies[i].at, payload: b }
    }

    fn write_move(&self, b: &mut Vec<u8>, i: usize) {
        let p = &self.plies[i];
        // At most MAX_PLIES plies.
        b.extend_from_slice(&(i as u16).to_le_bytes());
        b.extend_from_slice(&p.mv.to_le_bytes());
        b.push(p.flags);
        b.push(p.bits);
        b.extend_from_slice(&[0, 0]);
        b.extend_from_slice(&p.spent.to_le_bytes());
        b.extend_from_slice(&p.clock_after.to_le_bytes());
        b.extend_from_slice(&p.quota_after.to_le_bytes());
        b.extend_from_slice(&p.gseq.to_le_bytes());
        b.extend_from_slice(&p.at.to_le_bytes());
    }

    /// An event record (12 bytes) with the current gseq.
    pub(crate) fn event_record(
        &self,
        kind: EventKind,
        side: Option<Side>,
        arg: i64,
        at: i64,
    ) -> JournalRecord {
        let mut b = Vec::with_capacity(EVENT_REC_BYTES);
        b.extend_from_slice(&[kind as u8, Side::code(side), 0, 0]);
        b.extend_from_slice(&self.gseq.to_le_bytes());
        b.extend_from_slice(&u32_field(arg).to_le_bytes());
        JournalRecord { kind: RecordKind::Event, at, payload: b }
    }

    /// The `recovered` event record (16 bytes) with the current gseq.
    pub(crate) fn recovered_record(&self, grace: i64, at: i64, hold: i64, rec_flags: u8) -> JournalRecord {
        let mut b = Vec::with_capacity(RECOVERED_REC_BYTES);
        b.extend_from_slice(&[EventKind::Recovered as u8, Side::code(None), rec_flags, 0]);
        b.extend_from_slice(&self.gseq.to_le_bytes());
        b.extend_from_slice(&u32_field(grace).to_le_bytes());
        b.extend_from_slice(&u32_field(hold).to_le_bytes());
        JournalRecord { kind: RecordKind::Event, at, payload: b }
    }

    /// The `ended` record.
    pub(crate) fn ended_record(&self) -> JournalRecord {
        let r = self.result.expect("an ended record is only written for a finished game");
        let mut b = Vec::with_capacity(ENDED_REC_BYTES);
        b.extend_from_slice(&[r.status.to_u8(), r.reason.to_u8(), Side::code(self.culprit), 0]);
        b.extend_from_slice(&r.white_ms.to_le_bytes());
        b.extend_from_slice(&r.black_ms.to_le_bytes());
        b.extend_from_slice(&self.end_gseq.to_le_bytes());
        b.extend_from_slice(&r.ended_at.to_le_bytes());
        JournalRecord { kind: RecordKind::Ended, at: r.ended_at, payload: b }
    }

    /// Every counter the move records do not carry (68 bytes), at `at` (the turn start in
    /// [`GameRoom::journal_state`], the processing time at the end of a clock hold):
    /// `u8 7 | u8 drawOffer | u8 presence (1 White connected, 2 Black connected, 4 clock held,
    /// 8 / 16 White / Black away since a recovery) | u8 0 | u32 gseq | u16 drawOffersUsed W |
    /// u16 B | i32 drawDeclinedAt W | i32 B | u16 desyncs W | u16 B | u32 record flags |
    /// i64 disconnectedAt W | i64 B | i64 turnStart | u32 quota W | u32 B | u32 grace W | u32 B`.
    pub(crate) fn checkpoint_record(&self, at: i64) -> JournalRecord {
        let presence = (if self.connected[0] { CP_WHITE_CONNECTED } else { 0 })
            | (if self.connected[1] { CP_BLACK_CONNECTED } else { 0 })
            | (if self.clock_held { CP_CLOCK_HELD } else { 0 })
            | (if self.away_since_recovery[0] { CP_WHITE_RECOVERY_AWAY } else { 0 })
            | (if self.away_since_recovery[1] { CP_BLACK_RECOVERY_AWAY } else { 0 });
        let mut b = Vec::with_capacity(CHECKPOINT_BYTES);
        b.extend_from_slice(&[EventKind::Checkpoint as u8, Side::code(self.draw_offer), presence, 0]);
        b.extend_from_slice(&self.gseq.to_le_bytes());
        for v in self.draw_offers_used {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.draw_declined_at {
            b.extend_from_slice(&v.to_le_bytes());
        }
        for v in self.desyncs {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&u32_field(self.flags).to_le_bytes());
        for v in self.disconnected_at {
            b.extend_from_slice(&v.to_le_bytes());
        }
        b.extend_from_slice(&self.clock.turn_start().to_le_bytes());
        for s in Side::BOTH {
            b.extend_from_slice(&u32_field(self.clock.quota(s)).to_le_bytes());
        }
        for v in self.disconnect_grace {
            b.extend_from_slice(&u32_field(v).to_le_bytes());
        }
        debug_assert_eq!(b.len(), CHECKPOINT_BYTES);
        JournalRecord { kind: RecordKind::Event, at, payload: b }
    }

    fn replay_move(&mut self, b: &[u8]) -> Result<(), ReplayError> {
        if self.is_over() {
            return Err(ReplayError::new("move after the end"));
        }
        let mut r = Reader::new(b, "move");
        let ply = usize::from(r.u16()?);
        let mv = r.u16()?;
        let flags = r.u8()?;
        let bits = r.u8()?;
        r.u16()?;
        let rec = PlyRecord {
            mv,
            flags,
            bits,
            spent: r.u32()?,
            clock_after: r.u32()?,
            quota_after: r.u32()?,
            gseq: r.u32()?,
            at: r.i64()?,
        };
        if ply != self.ply() {
            return Err(ReplayError::new(format!("move for ply {ply} at ply {}", self.ply())));
        }
        if self.rules.play(mv).is_none() {
            return Err(ReplayError::new(format!("move {mv} refused by the rules at ply {ply}")));
        }
        self.apply_move(rec);
        self.gseq = rec.gseq.wrapping_add(u32::from(bits & super::MB_DECLINED != 0));
        Ok(())
    }

    fn replay_event(&mut self, b: &[u8], at: i64) -> Result<(), ReplayError> {
        let mut r = Reader::new(b, "event");
        let kind = r.u8()?;
        if kind == EventKind::Checkpoint as u8 {
            return self.replay_checkpoint(b);
        }
        let color = r.u8()?;
        let rec_flags = r.u8()?;
        r.u8()?;
        let gseq = r.u32()?;
        let arg = i64::from(r.u32()?);
        if self.is_over() {
            // Nothing is journaled after the end; ignored defensively.
            return Ok(());
        }
        let kind =
            EventKind::from_u8(kind).ok_or_else(|| ReplayError::new(format!("unknown event kind {kind}")))?;
        if kind == EventKind::Recovered {
            let hold = i64::from(r.u32()?);
            self.apply_event(kind, None, arg, at, hold, rec_flags & REC_FIRST_MOVE_RESTART);
        } else {
            let side =
                Side::from_code(color).ok_or_else(|| ReplayError::new(format!("bad colour {color}")))?;
            self.apply_event(kind, Some(side), arg, at, 0, 0);
        }
        self.gseq = gseq;
        Ok(())
    }

    fn replay_checkpoint(&mut self, b: &[u8]) -> Result<(), ReplayError> {
        let mut r = Reader::new(b, "checkpoint");
        if b.len() < CHECKPOINT_BYTES {
            return Err(ReplayError::new("short checkpoint record"));
        }
        r.u8()?;
        let draw_offer = Side::from_code(r.u8()?);
        let presence = r.u8()?;
        r.u8()?;
        self.draw_offer = draw_offer;
        self.connected = [presence & CP_WHITE_CONNECTED != 0, presence & CP_BLACK_CONNECTED != 0];
        self.clock_held = presence & CP_CLOCK_HELD != 0;
        self.away_since_recovery =
            [presence & CP_WHITE_RECOVERY_AWAY != 0, presence & CP_BLACK_RECOVERY_AWAY != 0];
        self.gseq = r.u32()?;
        self.draw_offers_used = [r.u16()?, r.u16()?];
        self.draw_declined_at = [r.i32()?, r.i32()?];
        self.desyncs = [r.u16()?, r.u16()?];
        self.flags = i64::from(r.u32()?);
        self.disconnected_at = [r.i64()?, r.i64()?];
        self.clock.restart(r.i64()?);
        for s in Side::BOTH {
            self.clock.set_quota(s, i64::from(r.u32()?));
        }
        self.disconnect_grace = [i64::from(r.u32()?), i64::from(r.u32()?)];
        Ok(())
    }

    fn replay_ended(&mut self, b: &[u8]) -> Result<(), ReplayError> {
        if self.is_over() {
            return Err(ReplayError::new("second ended record"));
        }
        let mut r = Reader::new(b, "ended");
        let status = r.u8()?;
        let reason = r.u8()?;
        let culprit = Side::from_code(r.u8()?);
        r.u8()?;
        let (white_ms, black_ms, end_gseq, ended_at) = (r.u32()?, r.u32()?, r.u32()?, r.i64()?);
        let status = GameStatus::from_u8(status)
            .filter(|&s| s != GameStatus::Ongoing)
            .ok_or_else(|| ReplayError::new(format!("bad status {status}")))?;
        let reason = Some(EndReason::from_u8(reason))
            .filter(|r| r.is_known())
            .ok_or_else(|| ReplayError::new(format!("bad end reason {reason}")))?;
        self.apply_end(GameResult { status, reason, white_ms, black_ms, ended_at }, culprit);
        self.end_gseq = end_gseq;
        self.gseq = end_gseq;
        Ok(())
    }
}
