//! Metrics of the realtime layer, under the names of the former Node.js server (router and
//! control plane). The former per-shard gauges carry no `shard` label any more: there is one
//! process. `scacelith_ws_relayed_total` (game messages relayed over the shard bus) is gone with
//! the bus.

use std::sync::{LazyLock, OnceLock};

use scacelith_protocol::MsgType;

use crate::metrics::{self, Counter, CounterVec, Gauge, Histogram};

/// `scacelith_ws_hello_ms` buckets.
const HELLO_BUCKETS: [f64; 10] = [1.0, 2.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];
/// `scacelith_ws_rtt_ms` buckets.
const RTT_BUCKETS: [f64; 11] = [5.0, 10.0, 25.0, 50.0, 75.0, 100.0, 150.0, 250.0, 500.0, 1000.0, 2000.0];

/// The connection-side metrics (the former router).
pub(crate) struct ConnMetrics {
    pub connections: Gauge,
    pub players: Gauge,
    messages_in: CounterVec,
    /// Children of `messages_in` by type byte, created on first use.
    by_type: Box<[OnceLock<Counter>; 256]>,
    pub hello: CounterVec,
    pub hello_ms: Histogram,
    pub rtt: Histogram,
    pub drop_rate: Counter,
    pub drop_seq: Counter,
    pub drop_ping: Counter,
    pub anomalies: CounterVec,
    pub g_drop_rate: Counter,
    pub g_drop_not_attached: Counter,
    pub g_drop_no_game: Counter,
}

impl ConnMetrics {
    /// Counts a message received, by its type name (`0x..` for a byte that names no type).
    pub fn count_in(&self, type_byte: u8) {
        self.by_type[usize::from(type_byte)]
            .get_or_init(|| match MsgType::from_u8(type_byte) {
                Some(t) => self.messages_in.with(&[t.name()]),
                None => self.messages_in.with(&[&format!("0x{type_byte:x}")]),
            })
            .inc();
    }
}

/// The connection-side metrics.
pub(crate) fn conn() -> &'static ConnMetrics {
    static M: LazyLock<ConnMetrics> = LazyLock::new(|| {
        let dropped =
            metrics::counter_vec("scacelith_ws_dropped_total", "Client messages dropped", &["reason"]);
        // Shared with the game hosts, which count the other reasons.
        let g_dropped = metrics::counter_vec(
            "scacelith_gestures_dropped_total",
            "Gestures not relayed, by reason",
            &["reason"],
        );
        ConnMetrics {
            connections: metrics::gauge("scacelith_ws_connections", "Open WebSocket connections"),
            players: metrics::gauge("scacelith_ws_players", "Authenticated connections"),
            messages_in: metrics::counter_vec(
                "scacelith_ws_messages_in_total",
                "Messages received by type",
                &["type"],
            ),
            by_type: Box::new(std::array::from_fn(|_| OnceLock::new())),
            hello: metrics::counter_vec("scacelith_ws_hello_total", "Hello outcomes", &["result"]),
            hello_ms: metrics::histogram(
                "scacelith_ws_hello_ms",
                "Connection open to Welcome (token check and presence included)",
                &HELLO_BUCKETS,
            ),
            rtt: metrics::histogram(
                "scacelith_ws_rtt_ms",
                "Round trip measured with the heartbeat",
                &RTT_BUCKETS,
            ),
            drop_rate: dropped.with(&["rate"]),
            drop_seq: dropped.with(&["bad_seq"]),
            drop_ping: dropped.with(&["ping_limit"]),
            anomalies: metrics::counter_vec(
                "scacelith_ws_anomalies_total",
                "Protocol anomalies seen by the router",
                &["kind"],
            ),
            g_drop_rate: g_dropped.with(&["rate"]),
            g_drop_not_attached: g_dropped.with(&["not_attached"]),
            g_drop_no_game: g_dropped.with(&["no_game"]),
        }
    });
    &M
}

/// The lobby's metrics (the former control plane).
pub(crate) struct LobbyMetrics {
    pub online: Gauge,
    pub connections: Gauge,
    pub searching: Gauge,
    pub challenges_open: Gauge,
    pub created: CounterVec,
    pub create_failed: Counter,
    pub kicks: CounterVec,
}

/// The lobby's metrics.
pub(crate) fn lobby() -> &'static LobbyMetrics {
    static M: LazyLock<LobbyMetrics> = LazyLock::new(|| LobbyMetrics {
        online: metrics::gauge("scacelith_presence_online", "Authenticated players online"),
        connections: metrics::gauge(
            "scacelith_presence_connections",
            "WebSocket connections counted for MAX_CONNECTIONS",
        ),
        searching: metrics::gauge("scacelith_mm_searching", "Players in the matchmaking queues"),
        challenges_open: metrics::gauge("scacelith_challenges_open", "Open challenges and private codes"),
        created: metrics::counter_vec(
            "scacelith_games_created_total",
            "Games created by the lobby",
            &["source"],
        ),
        create_failed: metrics::counter("scacelith_games_create_failed_total", "Game creations that failed"),
        kicks: metrics::counter_vec(
            "scacelith_presence_kicks_total",
            "Connections kicked by the lobby",
            &["reason"],
        ),
    });
    &M
}
