//! Connection counts behind the WebSocket upgrade (the former `presence.js` counts and the
//! router's `isFull`): `MAX_CONNECTIONS_PER_IP` per IPv4 address or IPv6 /64, and the whole
//! server's connections.
//!
//! `MAX_CONNECTIONS` has two checks. At the upgrade the user is not known yet, so the count may go
//! a small reserve beyond it ([`upgrade_reserve`]: max(16, 2 %)); the exact cap is applied at
//! Hello on the online players, where the lobby refuses newcomers (`ServerFull`, close 4006) but
//! still admits a player whose game is in progress. A player who lost the connection during a
//! game on a full server can thus come back before the reconnection grace runs out.
//!
//! The server-full signal ([`Admissions::full_signal`]) makes the TLS gate shed new connections
//! before the handshake. It is true while the last refusal of the global check (beyond the
//! reserve) is newer than the last admitted upgrade and less than [`FULL_HOLD_MS`] old, or while
//! the server holds 1.2 times `MAX_CONNECTIONS`. A `ServerFull` at Hello deliberately does not
//! start it: a server at `MAX_CONNECTIONS` itself does not shed, so that a returning player does
//! not compete with newcomers for the handshake slots.
//!
//! Every count is O(1) under one short lock; a count is released when the connection's
//! [`AdmissionPermit`] is dropped. The server-full signal takes no lock: the TLS gate calls it
//! under its own mutex, so it reads atomics written under the count lock.

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use parking_lot::Mutex;

use super::metrics;
use crate::clock::SharedClock;
use crate::config::Config;
use crate::net::gate::FullSignal;
use crate::net::ip::AddrKey;
use crate::net::upgrade::{Admission, AdmissionRefusal};
use crate::net::ws::AdmissionPermit;

/// How long a server-full refusal at the upgrade keeps the server-full signal on.
pub const FULL_HOLD_MS: f64 = 5000.0;

/// Upgrades admitted beyond `MAX_CONNECTIONS`, for the players with a game in progress.
pub fn upgrade_reserve(max_connections: usize) -> usize {
    max_connections.div_ceil(50).max(16)
}

/// Why an upgrade was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// The address (or its /64) holds `MAX_CONNECTIONS_PER_IP` connections (HTTP 429).
    PerIp,
    /// The server holds `MAX_CONNECTIONS` plus the reserve (HTTP 503).
    Global,
}

/// A monotonic time in milliseconds read without a lock; minus infinity until first set.
struct Stamp(AtomicU64);

impl Stamp {
    fn never() -> Stamp {
        Stamp(AtomicU64::new(f64::NEG_INFINITY.to_bits()))
    }

    fn set(&self, ms: f64) {
        self.0.store(ms.to_bits(), Ordering::Relaxed);
    }

    fn get(&self) -> f64 {
        f64::from_bits(self.0.load(Ordering::Relaxed))
    }
}

struct Inner {
    /// Connections per address group. Its lock also orders the writes of the three atomics
    /// below, which the server-full signal reads without it.
    by_key: Mutex<HashMap<AddrKey, u32>>,
    connections: AtomicUsize,
    /// The last global refusal and the last admitted upgrade.
    full_at: Stamp,
    admit_at: Stamp,
    max_per_ip: u32,
    upgrade_cap: usize,
    shed_at: usize,
    clock: SharedClock,
}

/// The connection counts of the WebSocket upgrades. Cheap to clone.
#[derive(Clone)]
pub struct Admissions {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Admissions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Admissions")
            .field("connections", &self.connections())
            .field("upgrade_cap", &self.inner.upgrade_cap)
            .finish()
    }
}

impl Admissions {
    /// Counts for `MAX_CONNECTIONS` and `MAX_CONNECTIONS_PER_IP` of `config`.
    pub fn from_config(config: &Config, clock: SharedClock) -> Admissions {
        let max = usize::try_from(config.max_connections).unwrap_or(0);
        let per_ip = u32::try_from(config.max_connections_per_ip).unwrap_or(u32::MAX);
        Admissions::new(max, per_ip, clock)
    }

    /// Counts for `max_connections` players and `max_per_ip` connections per address.
    pub fn new(max_connections: usize, max_per_ip: u32, clock: SharedClock) -> Admissions {
        Admissions {
            inner: Arc::new(Inner {
                by_key: Mutex::new(HashMap::new()),
                connections: AtomicUsize::new(0),
                full_at: Stamp::never(),
                admit_at: Stamp::never(),
                max_per_ip,
                upgrade_cap: max_connections + upgrade_reserve(max_connections),
                shed_at: (max_connections * 6).div_ceil(5),
                clock,
            }),
        }
    }

    /// Connections admitted at the upgrade: `MAX_CONNECTIONS` plus the reserve.
    pub fn upgrade_cap(&self) -> usize {
        self.inner.upgrade_cap
    }

    /// Open connections counted.
    pub fn connections(&self) -> usize {
        self.inner.connections.load(Ordering::Relaxed)
    }

    /// Open connections counted for the group of `ip`.
    pub fn count_of(&self, ip: IpAddr) -> u32 {
        self.inner.by_key.lock().get(&AddrKey::of(ip)).copied().unwrap_or(0)
    }

    /// Counts a new connection from `ip` unless a limit is reached (the global one first). The
    /// count is released when the returned slot is dropped.
    pub fn try_acquire(&self, ip: IpAddr) -> Result<Slot, Refusal> {
        let now = self.inner.clock.mono_ms();
        let key = AddrKey::of(ip);
        let inner = &*self.inner;
        let mut by_key = inner.by_key.lock();
        let connections = inner.connections.load(Ordering::Relaxed);
        if connections >= inner.upgrade_cap {
            inner.full_at.set(now);
            return Err(Refusal::Global);
        }
        let n = by_key.entry(key).or_insert(0);
        if *n >= inner.max_per_ip {
            if *n == 0 {
                by_key.remove(&key);
            }
            return Err(Refusal::PerIp);
        }
        *n += 1;
        inner.connections.store(connections + 1, Ordering::Relaxed);
        inner.admit_at.set(now);
        metrics::lobby().connections.set((connections + 1) as f64);
        Ok(Slot { owner: self.clone(), key })
    }

    fn release(&self, key: AddrKey) {
        let inner = &*self.inner;
        let mut by_key = inner.by_key.lock();
        if let Some(n) = by_key.get_mut(&key) {
            *n -= 1;
            if *n == 0 {
                by_key.remove(&key);
            }
            let connections = inner.connections.load(Ordering::Relaxed).saturating_sub(1);
            inner.connections.store(connections, Ordering::Relaxed);
            metrics::lobby().connections.set(connections as f64);
        }
    }

    /// Whether new connections should be shed before the TLS handshake (module documentation).
    /// Lock-free: three atomic reads and the clock.
    pub fn is_full(&self) -> bool {
        let inner = &*self.inner;
        if inner.connections.load(Ordering::Relaxed) >= inner.shed_at {
            return true;
        }
        let full = inner.full_at.get();
        full > inner.admit_at.get() && (0.0..FULL_HOLD_MS).contains(&(inner.clock.mono_ms() - full))
    }

    /// [`Admissions::is_full`] as the gate's signal.
    pub fn full_signal(&self) -> FullSignal {
        let me = self.clone();
        Arc::new(move || me.is_full())
    }
}

/// One counted connection; dropping it releases the count.
#[derive(Debug)]
pub struct Slot {
    owner: Admissions,
    key: AddrKey,
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.owner.release(self.key);
    }
}

impl Admission for Admissions {
    fn acquire(&self, ip: IpAddr) -> Result<AdmissionPermit, AdmissionRefusal> {
        match self.try_acquire(ip) {
            Ok(slot) => Ok(AdmissionPermit::new(slot)),
            Err(Refusal::PerIp) => Err(AdmissionRefusal::too_many_connections()),
            Err(Refusal::Global) => Err(AdmissionRefusal::server_full()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::ManualClock;

    fn ip(s: &str) -> IpAddr {
        crate::net::ip::normalize_ip(s).expect("an address")
    }

    fn admissions(max: usize, per_ip: u32) -> (Admissions, Arc<ManualClock>) {
        let clock = ManualClock::new(1_000_000.0, 0);
        (Admissions::new(max, per_ip, clock.clone()), clock)
    }

    #[test]
    fn limits_connections_per_ipv4_address_and_per_ipv6_64() {
        let (a, _) = admissions(100, 2);
        let mut held = Vec::new();
        held.push(a.try_acquire(ip("192.0.2.1")).expect("first"));
        held.push(
            a.try_acquire(ip("::ffff:192.0.2.1")).expect("the same client through a dual-stack listener"),
        );
        assert_eq!(a.try_acquire(ip("192.0.2.1")).unwrap_err(), Refusal::PerIp);
        held.push(a.try_acquire(ip("192.0.2.2")).expect("another address"));
        let first64 = a.try_acquire(ip("2001:db8:0:1::1")).expect("v6");
        held.push(a.try_acquire(ip("2001:db8:0:1:ffff::2")).expect("same /64"));
        assert_eq!(a.try_acquire(ip("2001:db8:0:1:abcd:1:2:3")).unwrap_err(), Refusal::PerIp);
        held.push(a.try_acquire(ip("2001:db8:0:2::1")).expect("another /64"));
        drop(first64);
        held.push(a.try_acquire(ip("2001:db8:0:1::99")).expect("released"));
        assert_eq!(a.count_of(ip("2001:db8:0:1::5")), 2);
        assert_eq!(a.connections(), held.len());
        drop(held);
        assert_eq!(a.connections(), 0);
        // No count per /48: each /64 of a /48 has the whole limit.
        let (q, _) = admissions(100, 2);
        let mut held = Vec::new();
        for net in 1..=6 {
            for i in 1..=2 {
                held.push(q.try_acquire(ip(&format!("2001:db8:5:{net}::{i}"))).expect("within its /64"));
            }
        }
        assert_eq!(q.connections(), 12);
    }

    #[test]
    fn enforces_the_global_limit_with_its_reserve() {
        // Upgrades may go 16 beyond MAX_CONNECTIONS (the players with a game in progress; the
        // lobby applies the exact cap at Hello).
        let (a, _) = admissions(3, 10);
        assert_eq!(a.upgrade_cap(), 3 + 16);
        let mut held: Vec<Slot> =
            (1..=3).map(|i| a.try_acquire(ip(&format!("10.0.0.{i}"))).expect("slot")).collect();
        let reserve: Vec<Slot> =
            (0..16).map(|i| a.try_acquire(ip(&format!("10.0.1.{i}"))).expect("reserve")).collect();
        assert_eq!(a.try_acquire(ip("10.0.0.4")).unwrap_err(), Refusal::Global);
        drop(reserve);
        assert_eq!(a.connections(), 3);
        held.push(a.try_acquire(ip("10.0.0.4")).expect("room again"));
    }

    #[test]
    fn sizes_the_upgrade_reserve() {
        let sizes: Vec<usize> = [1, 100, 800, 801, 1000, 200_000].into_iter().map(upgrade_reserve).collect();
        assert_eq!(sizes, [16, 16, 16, 17, 20, 4000]);
        assert_eq!(admissions(200_000, 16).0.upgrade_cap(), 204_000);
    }

    #[test]
    fn the_full_signal_follows_global_refusals_for_five_seconds() {
        // 100 + 16 upgrades fit, below the 120 that shed by themselves.
        let (a, clock) = admissions(100, 1000);
        let mut held: Vec<Slot> = (0..116).map(|_| a.try_acquire(ip("10.0.0.1")).expect("slot")).collect();
        assert!(!a.is_full(), "no global refusal yet");
        clock.advance(10.0);
        assert_eq!(a.try_acquire(ip("10.0.0.2")).unwrap_err(), Refusal::Global);
        assert!(a.is_full());
        clock.advance(4999.0);
        assert!(a.is_full());
        clock.advance(1.0);
        assert!(!a.is_full(), "the hold is over");
        assert_eq!(a.try_acquire(ip("10.0.0.2")).unwrap_err(), Refusal::Global);
        assert!(a.is_full());
        held.pop();
        clock.advance(10.0);
        held.push(a.try_acquire(ip("10.0.0.3")).expect("room again"));
        assert!(!a.is_full(), "an admitted upgrade ends it");
    }

    #[test]
    fn sheds_at_one_point_two_times_max_connections() {
        // Reachable when the reserve exceeds 20 % (MAX_CONNECTIONS up to 80).
        let (a, _) = admissions(10, 1000);
        let held: Vec<Slot> = (0..11).map(|_| a.try_acquire(ip("10.0.0.1")).expect("slot")).collect();
        assert!(!a.is_full());
        let extra = a.try_acquire(ip("10.0.0.1")).expect("slot 12");
        assert!(a.is_full());
        drop(extra);
        assert!(!a.is_full());
        drop(held);
    }

    #[test]
    fn answers_the_upgrade_with_429_or_503() {
        let (a, _) = admissions(0, 100);
        let held: Vec<AdmissionPermit> =
            (0..16).map(|_| Admission::acquire(&a, ip("10.0.0.1")).expect("reserve")).collect();
        let refusal = Admission::acquire(&a, ip("10.0.0.1")).unwrap_err();
        assert_eq!((refusal.status, refusal.error.as_ref()), (503, "server_full"));
        drop(held);
        let (b, _) = admissions(10, 1);
        let permit = Admission::acquire(&b, ip("10.0.0.1")).expect("admitted");
        let refusal = Admission::acquire(&b, ip("10.0.0.1")).unwrap_err();
        assert_eq!((refusal.status, refusal.error.as_ref()), (429, "too_many_connections"));
        drop(permit);
        assert_eq!(b.connections(), 0, "the permit releases its count");
    }
}
