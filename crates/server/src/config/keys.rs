//! The configuration key table: every setting an administrator can change, with its section,
//! type, default, bounds and description. It is the single source of truth of the loader
//! (`load.rs`), of `check-config` and of the generated `.env.example` and `docs/CONFIG.md`
//! (`scacelith-server gen-config-docs`).
//!
//! Defaults are written as the text an administrator would put in `.env`; a key without default
//! is either unset (`null` in `check-config`) or derived at load time, as its description says.
//! Settings that the former server applied per worker process (handshake slots, password hash
//! slots, GIF threads, caches) are whole-server values here, and their defaults scale with
//! `WORKERS` so that a server keeps the capacity it had.

/// A group of keys, one block of `.env.example` and one table of `docs/CONFIG.md`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Section {
    /// Server identity and network.
    Server,
    /// TLS.
    Tls,
    /// Storage.
    Storage,
    /// Secrets.
    Secrets,
    /// Accounts.
    Accounts,
    /// Mail.
    Mail,
    /// Google single sign-on.
    Sso,
    /// Protection per address (background layer).
    Abuse,
    /// Abuse protection and limits.
    Limits,
    /// Games.
    Games,
    /// Matchmaking and ratings.
    Matchmaking,
    /// Anti-cheat and sanctions.
    Anticheat,
    /// Animated GIFs of games.
    Gif,
    /// Observability.
    Observability,
}

impl Section {
    /// Title of the section in the generated files.
    pub fn title(self) -> &'static str {
        match self {
            Section::Server => "Server identity and network",
            Section::Tls => "TLS",
            Section::Storage => "Storage",
            Section::Secrets => "Secrets",
            Section::Accounts => "Accounts",
            Section::Mail => "Mail",
            Section::Sso => "Google single sign-on",
            Section::Abuse => "Protection per address (background layer)",
            Section::Limits => "Abuse protection and limits",
            Section::Games => "Games",
            Section::Matchmaking => "Matchmaking and ratings",
            Section::Anticheat => "Anti-cheat and sanctions",
            Section::Gif => "Animated GIFs of games",
            Section::Observability => "Observability",
        }
    }
}

/// How a key's text is read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Text used as written (not trimmed).
    Text,
    /// A path, resolved against the working directory (lexically, symbolic links untouched).
    Path,
    /// A decimal integer.
    Int,
    /// A TCP port, 0 to 65535.
    Port,
    /// A decimal number.
    Number,
    /// `true/false`, `1/0`, `yes/no`, `on/off` (any case).
    Bool,
    /// One of the listed values (case-sensitive).
    Enum(&'static [&'static str]),
    /// Comma-separated values, each trimmed, empty ones dropped.
    List,
    /// A binary secret: hexadecimal, or base64 decoded leniently as the former server did.
    Secret,
    /// A text secret used as written (passwords, client secrets, tokens).
    SecretText,
}

impl Kind {
    /// Whether the key is a secret (hidden by `check-config`, may come from `<KEY>_FILE`).
    pub fn is_secret(self) -> bool {
        matches!(self, Kind::Secret | Kind::SecretText)
    }
}

/// One configuration key.
#[derive(Clone, Copy, Debug)]
pub struct KeySpec {
    /// Environment variable name.
    pub name: &'static str,
    /// Group of the generated files.
    pub section: Section,
    /// Type.
    pub kind: Kind,
    /// Default as `.env` text; `None` when unset by default (or derived, see the description).
    pub default: Option<&'static str>,
    /// Smallest accepted value (`Int`, `Number`).
    pub min: Option<i64>,
    /// Largest accepted value (`Int`, `Number`).
    pub max: Option<i64>,
    /// Longest text in UTF-16 code units, as JavaScript counted characters (`Text`).
    pub max_chars: Option<usize>,
    /// Longest text in UTF-8 bytes; such a key also refuses NUL characters (`Text`).
    pub max_bytes: Option<usize>,
    /// Fewest decoded bytes (`Secret`).
    pub min_bytes: Option<usize>,
    /// The server does not start without it.
    pub required: bool,
    /// Description for the generated files. Its first sentence also explains a missing required
    /// key.
    pub desc: &'static str,
}

const fn key(name: &'static str, section: Section, kind: Kind, desc: &'static str) -> KeySpec {
    KeySpec {
        name,
        section,
        kind,
        default: None,
        min: None,
        max: None,
        max_chars: None,
        max_bytes: None,
        min_bytes: None,
        required: false,
        desc,
    }
}

impl KeySpec {
    const fn default(mut self, text: &'static str) -> KeySpec {
        self.default = Some(text);
        self
    }

    const fn min(mut self, v: i64) -> KeySpec {
        self.min = Some(v);
        self
    }

    const fn max(mut self, v: i64) -> KeySpec {
        self.max = Some(v);
        self
    }

    const fn range(self, min: i64, max: i64) -> KeySpec {
        self.min(min).max(max)
    }

    const fn max_chars(mut self, n: usize) -> KeySpec {
        self.max_chars = Some(n);
        self
    }

    const fn max_bytes(mut self, n: usize) -> KeySpec {
        self.max_bytes = Some(n);
        self
    }

    const fn min_bytes(mut self, n: usize) -> KeySpec {
        self.min_bytes = Some(n);
        self
    }

    const fn required(mut self) -> KeySpec {
        self.required = true;
        self
    }
}

/// The key of this name, if any.
pub fn spec(name: &str) -> Option<&'static KeySpec> {
    KEYS.iter().find(|k| k.name == name)
}

/// Keys of the former Node.js server that no longer exist, with the warning `check-config`
/// prints when one is still set (in the environment or the `.env` file). They never stop the
/// server: an existing `.env` keeps working.
pub const OBSOLETE: &[(&str, &str)] = &[
    (
        "WS_MAX_MESSAGE_BYTES",
        "WS_MAX_MESSAGE_BYTES is no longer used: protocol v1 fixes the largest client message at 512 bytes \
         (docs/PROTOCOL.md). Remove it.",
    ),
    (
        "GOOGLE_REDIRECT_URI",
        "GOOGLE_REDIRECT_URI is no longer used: Google sign-in now returns to the game on 127.0.0.1. Remove it \
         and use a \"Desktop app\" OAuth client.",
    ),
    (
        "SHARD_OVERLOAD_LAG_MS",
        "SHARD_OVERLOAD_LAG_MS is no longer used: the shards of this server share the threads of one process, \
         so no shard is overloaded alone. Remove it.",
    ),
    (
        "LISTEN_REUSE_PORT",
        "LISTEN_REUSE_PORT is no longer used: one process accepts every connection. Remove it.",
    ),
    (
        "UV_THREADPOOL_SIZE",
        "UV_THREADPOOL_SIZE is a Node.js setting and has no effect on this server: PASSWORD_HASH_CONCURRENCY \
         alone bounds the password hashes. Remove it.",
    ),
];

use Kind::*;
use Section::*;

/// Every configuration key, in the order of the generated files and of `check-config`.
pub static KEYS: &[KeySpec] = &[
    // ---- Server identity and network --------------------------------------------------------
    key("SERVER_NAME", Server, Text, "Name shown to players (menus, scoresheet \"Event\").")
        .default("Scacelith Community Server")
        .max_chars(64)
        .max_bytes(64),
    key(
        "SERVER_PUBLIC_HOST",
        Server,
        Text,
        "Public DNS name of the server, used in e-mail links, and Google sign-in works only for players who \
         added the server under exactly this name and PUBLIC_API_PORT.",
    )
    .default("localhost"),
    key("SERVER_MOTD", Server, Text, "Short message of the day shown in the online menu.").default("").max_chars(200),
    key("BIND_ADDRESS", Server, Text, "Address the API and WebSocket listeners bind to.").default("0.0.0.0"),
    key(
        "API_PORT",
        Server,
        Port,
        "HTTPS API port (TCP). 443, the HTTPS port: firewalls and proxies let it through; any free port works \
         for a community server. A port below 1024 needs the CAP_NET_BIND_SERVICE capability (docs/DEPLOY.md) \
         unless the server runs as root.",
    )
    .default("443"),
    key(
        "WS_PORT",
        Server,
        Port,
        "WSS (game WebSocket) port. Empty (the default) = the same port as API_PORT: one TLS listener serves \
         the API under /api/v1 and the WebSocket upgrade on /ws. Set another port to split them.",
    ),
    key("PUBLIC_API_PORT", Server, Port, "API port as seen by clients when a proxy/NAT maps ports (0 = API_PORT).")
        .default("0"),
    key("PUBLIC_WS_PORT", Server, Port, "WSS port as seen by clients (0 = WS_PORT).").default("0"),
    key(
        "WORKERS",
        Server,
        Text,
        "Game shards, and threads of the server's async runtime: a number from 1 to 64, or \"auto\" (one per \
         CPU core, at most 16). Each shard hosts its part of the games; the threads serve the connections, \
         the API and the games of every shard. Several defaults below scale with it.",
    )
    .default("auto"),
    key(
        "SHARD_BASE",
        Server,
        Int,
        "First shard number of this instance (multi-instance deployments give each instance its own range): \
         the shards are SHARD_BASE to SHARD_BASE + WORKERS - 1, and every game id holds the number of its \
         shard.",
    )
    .default("0")
    .range(0, 56),
    key("INSTANCE_ID", Server, Text, "Free label of this instance in logs and metrics (default: host name).")
        .default(""),
    key(
        "WS_ALLOWED_ORIGINS",
        Server,
        List,
        "Origin header values allowed to open the game WebSocket (e.g. https://play.example.org). The game \
         client sends no Origin; browsers always send one, so they are refused unless listed here.",
    )
    .default(""),
    key(
        "SHUTDOWN_GRACE_MS",
        Server,
        Int,
        "On SIGTERM/SIGINT players are warned (ServerShutdown notice) this long before their connections \
         close. Games in progress survive the restart (journal).",
    )
    .default("3000")
    .range(0, 120_000),
    key(
        "LISTEN_BACKLOG",
        Server,
        Int,
        "Length of the kernel queue of new connections not yet accepted (listen backlog), which absorbs \
         reconnection bursts. The kernel caps it at net.core.somaxconn (Linux), so raise that sysctl as well \
         (README, kernel settings).",
    )
    .default("2048")
    .range(128, 65_535),
    // ---- TLS --------------------------------------------------------------------------------
    key(
        "TLS_MODE",
        Tls,
        Enum(&["native", "proxy", "off"]),
        "native: this server terminates TLS with TLS_CERT_FILE/TLS_KEY_FILE. proxy: a reverse proxy (nginx, \
         haproxy, caddy) terminates TLS and forwards plain HTTP/WebSocket to this server on a private address. \
         off: plain text, refused unless ALLOW_INSECURE_DEV=1 (local development only).",
    )
    .default("native"),
    key(
        "TLS_CERT_FILE",
        Tls,
        Path,
        "PEM certificate chain (fullchain). Reloaded on SIGHUP and when the file changes.",
    )
    .default(""),
    key("TLS_KEY_FILE", Tls, Path, "PEM private key. Never commit it.").default(""),
    key("TLS_MIN_VERSION", Tls, Enum(&["TLSv1.2", "TLSv1.3"]), "Oldest TLS version accepted.").default("TLSv1.2"),
    key(
        "TRUSTED_PROXIES",
        Tls,
        List,
        "With TLS_MODE=proxy: addresses whose X-Forwarded-For header is trusted.",
    )
    .default("127.0.0.1,::1"),
    key(
        "ALLOW_INSECURE_DEV",
        Tls,
        Bool,
        "Allows TLS_MODE=off. Development only; never on a public server.",
    )
    .default("false"),
    // ---- Storage ----------------------------------------------------------------------------
    key(
        "DATA_DIR",
        Storage,
        Path,
        "Directory of the database and the game journal (created at start when missing).",
    )
    .default("./data"),
    key(
        "DB_PATH",
        Storage,
        Path,
        "SQLite database file (default: DATA_DIR/scacelith.db; :memory: keeps it in memory, for tests).",
    )
    .default(""),
    key(
        "JOURNAL_DIR",
        Storage,
        Path,
        "Append-only journal of the games in progress, replayed after a crash (default: DATA_DIR/journal).",
    )
    .default(""),
    key(
        "JOURNAL_FLUSH_MS",
        Storage,
        Int,
        "Longest time a game event waits in memory before being written to the journal (group commit).",
    )
    .default("50")
    .range(5, 1000),
    key(
        "JOURNAL_FSYNC",
        Storage,
        Bool,
        "fsync the journal at every flush (survives power loss, not only process crashes). With false, a flush \
         that holds a compaction snapshot is still fsynced (with the directory when its segment is new), \
         because the segments that snapshot replaces are deleted: a power loss then loses only the last \
         records written, never whole games.",
    )
    .default("true"),
    key(
        "JOURNAL_COMPACT_SEGMENTS",
        Storage,
        Int,
        "Journal compaction: once a shard's journal has moved this many 16 MB segments past the oldest record \
         a game still needs (its first record, or its latest snapshot), the game, running or waiting for its \
         database commit, is written again as one snapshot record and the older segments can be deleted. A \
         shard's journal then stays around (this + 1) x 16 MB however long the games last. Lower values write \
         more snapshots; higher ones keep more on disk and lengthen the replay after a restart.",
    )
    .default("4")
    .range(1, 1000),
    key(
        "DB_COMMIT_MS",
        Storage,
        Int,
        "Finished games are committed to the database in batches, at most this long after they end.",
    )
    .default("50")
    .range(1, 2000),
    key(
        "DB_CACHE_MB",
        Storage,
        Int,
        "SQLite page cache of the whole server, in megabytes, shared evenly among its database connections \
         (the writer and the readers). Empty (the default) = 64 x (WORKERS + 1).",
    )
    .range(2, 65_536),
    key(
        "DB_MMAP_MB",
        Storage,
        Int,
        "Part of the database file read through memory mapping, in megabytes (0 disables it).",
    )
    .default("256")
    .range(0, 65_536),
    // ---- Secrets ----------------------------------------------------------------------------
    key(
        "SERVER_SECRET",
        Secrets,
        Secret,
        "Master secret (at least 32 random bytes, hex or base64). Keys for proof-of-work challenges, \
         recovery-code hashing, OAuth state and MFA secret encryption are derived from it with HKDF. Generate \
         one with: scacelith-server gen-secret",
    )
    .required()
    .min_bytes(32),
    key(
        "MFA_ENCRYPTION_KEY",
        Secrets,
        Secret,
        "Optional separate key (32 bytes) encrypting TOTP secrets at rest. Default: derived from SERVER_SECRET. \
         Changing it makes existing TOTP enrolments unreadable.",
    )
    .default("")
    .min_bytes(32),
    // ---- Accounts ---------------------------------------------------------------------------
    key("REGISTRATION", Accounts, Enum(&["open", "closed"]), "Whether new accounts can be created from the game.")
        .default("open"),
    key(
        "REQUIRE_EMAIL_VERIFICATION",
        Accounts,
        Bool,
        "An account is created only once its e-mail address is confirmed with the link sent to it (24 h). \
         false: created at once, with no link.",
    )
    .default("true"),
    key("USERNAME_MIN", Accounts, Int, "Shortest username.").default("3").range(2, 24),
    key("USERNAME_MAX", Accounts, Int, "Longest username (the scoresheet has room for 24 characters).")
        .default("20")
        .range(3, 24),
    key("PASSWORD_MIN_LENGTH", Accounts, Int, "Shortest password.").default("10").range(8, 64),
    key("SESSION_IDLE_DAYS", Accounts, Int, "A session unused for this long expires.").default("30").range(1, 365),
    key("SESSION_MAX_DAYS", Accounts, Int, "A session expires this long after the login, even if used.")
        .default("90")
        .range(1, 730),
    key("MAX_SESSIONS_PER_USER", Accounts, Int, "Oldest sessions are revoked beyond this number.")
        .default("10")
        .range(1, 100),
    // ---- Mail -------------------------------------------------------------------------------
    key(
        "MAIL_TRANSPORT",
        Mail,
        Enum(&["smtp", "log", "none"]),
        "smtp: send with the SMTP_* settings. log: write the messages to the log (development). none: no mail \
         (then e-mail verification and password reset cannot work).",
    )
    .default("log"),
    key("MAIL_FROM", Mail, Text, "Sender address.").default("Scacelith <no-reply@localhost>"),
    key("SMTP_HOST", Mail, Text, "SMTP relay host.").default(""),
    key("SMTP_PORT", Mail, Port, "SMTP port (587 STARTTLS, 465 implicit TLS).").default("587"),
    key(
        "SMTP_SECURITY",
        Mail,
        Enum(&["starttls", "tls", "none"]),
        "starttls (required, not opportunistic), tls (implicit, port 465) or none (local relay only).",
    )
    .default("starttls"),
    key("SMTP_USER", Mail, Text, "SMTP user name (empty = no authentication).").default(""),
    key("SMTP_PASSWORD", Mail, SecretText, "SMTP password.").default(""),
    // ---- Google single sign-on --------------------------------------------------------------
    key(
        "SSO_GOOGLE_ENABLED",
        Sso,
        Bool,
        "Offers \"Sign in with Google\" (authorization code + PKCE; Google sends the browser back to the game \
         on 127.0.0.1 and the game hands the code to this server). An existing account is linked only after \
         its password, and its two-step code when on, is entered once in the game.",
    )
    .default("false"),
    key(
        "GOOGLE_CLIENT_ID",
        Sso,
        Text,
        "OAuth client ID of a \"Desktop app\" client (Google Auth Platform > Clients). Not a \"Web application\" \
         client: Google sends the browser back to the game on 127.0.0.1 and only a Desktop app client accepts \
         that.",
    )
    .default(""),
    key("GOOGLE_CLIENT_SECRET", Sso, SecretText, "OAuth client secret. Never commit it.").default(""),
    // ---- Protection per address (background layer) ------------------------------------------
    key(
        "HTTP_RATE_PER_IP",
        Abuse,
        Int,
        "HTTP requests per minute from one IPv4 address or IPv6 /64, whole server: every request (API, pages, \
         health checks, unknown paths, WebSocket upgrades), counted before routing and before \
         authentication, with a burst of half a minute. A background ceiling, not a quota: it is loose enough \
         for a school or a mobile operator that puts many players behind one address, and signed-in players \
         are limited per account as well. Beyond it: 429 rate_limited with Retry-After.",
    )
    .default("600")
    .min(1),
    key(
        "HTTP_RATE_PER_PREFIX",
        Abuse,
        Int,
        "The same for one IPv6 /48 as a whole, on top of the limit of each of its /64 networks: a /48 holds \
         65536 of them, and one customer often gets a /56 or a /48. 0 means 4 x HTTP_RATE_PER_IP; a value you \
         set must be at least HTTP_RATE_PER_IP. IPv4 addresses are only counted one by one.",
    )
    .default("0")
    .min(0),
    key(
        "IP_CONN_RATE",
        Abuse,
        Int,
        "New connections per second from one IPv4 address or IPv6 /64 (4 times that per /48), whole server, \
         with a burst of 4 seconds (TLS_MODE=native: checked before any TLS work; a connection beyond it is \
         closed with a reset). The game opens one or two connections per player.",
    )
    .default("10")
    .range(1, 100_000),
    key(
        "IP_MAX_CONNECTIONS",
        Abuse,
        Int,
        "Open connections (TLS handshakes, API keep-alive connections and WebSockets together) from one IPv4 \
         address or IPv6 /64 (4 times that per /48), whole server (TLS_MODE=native; a new connection beyond \
         it is closed with a reset before TLS). Keep it at least twice MAX_CONNECTIONS_PER_IP (check-config \
         warns otherwise).",
    )
    .default("128")
    .range(1, 1_000_000),
    key(
        "IP_MAX_INFLIGHT",
        Abuse,
        Int,
        "HTTP requests being processed at once for one IPv4 address or IPv6 /64 (4 times that per /48), whole \
         server; one more gets 429 rate_limited. Empty (the default) = 32 x WORKERS. It bounds what one \
         address can keep waiting (slow request bodies, the password hash queue).",
    )
    .range(1, 100_000),
    key(
        "ABUSE_BLOCK_REFUSALS_PER_MIN",
        Abuse,
        Int,
        "Refusals in one minute, whole server, that block an IPv4 address or IPv6 /64 before TLS (4 times that \
         for a /48): rate-limit 429s, connections refused before TLS, malformed requests and failed TLS \
         handshakes. A blocked address gets its new connections closed with a reset before any TLS work, and \
         429 with Connection: close on the connections it already has; WebSocket connections already open are \
         kept, so that the players of a school or a mobile operator keep their games when one of them floods. \
         The block starts at ABUSE_BLOCK_BASE_SEC and is 4 times longer at each repeat within 6 hours, up to \
         ABUSE_BLOCK_MAX_SEC. 0 = never block (the per-address limits still apply).",
    )
    .default("600")
    .min(0),
    key(
        "ABUSE_BLOCK_BASE_SEC",
        Abuse,
        Int,
        "First block of an address, in seconds (see ABUSE_BLOCK_REFUSALS_PER_MIN).",
    )
    .default("60")
    .range(1, 86_400),
    key(
        "ABUSE_BLOCK_MAX_SEC",
        Abuse,
        Int,
        "Longest block of an address, in seconds (at least ABUSE_BLOCK_BASE_SEC).",
    )
    .default("3600")
    .range(1, 604_800),
    key(
        "ABUSE_EXEMPT",
        Abuse,
        List,
        "Addresses and CIDR subnets (203.0.113.7, 2001:db8::/48) never blocked and outside HTTP_RATE_PER_IP, \
         IP_CONN_RATE, IP_MAX_CONNECTIONS and IP_MAX_INFLIGHT: a school or club network, monitoring, a load \
         generator. Login, registration, the other route limits and the per-account quotas still apply. An \
         invalid entry stops the start.",
    )
    .default(""),
    // ---- Abuse protection and limits --------------------------------------------------------
    key(
        "MAX_CONNECTIONS",
        Limits,
        Int,
        "Simultaneous players, whole server. A newcomer beyond it still completes the TLS handshake and the \
         WebSocket upgrade, then is refused at Hello (ServerFull, counted in \
         scacelith_ws_hello_total{result=\"server_full\"}, the metric that shows a full server), and the game \
         waits 60 to 120 s before it tries again. A player whose game is in progress is still admitted, so \
         that a full server does not make them lose it by abandonment. For such a player to reach Hello, \
         WebSocket upgrades may go max(16, 2 %) beyond it; beyond that reserve an upgrade gets HTTP 503, and \
         with TLS_MODE=native the TLS gate starts shedding (see MAX_PENDING_HANDSHAKES).",
    )
    .default("200000")
    .min(1),
    key(
        "MAX_CONNECTIONS_PER_IP",
        Limits,
        Int,
        "Simultaneous WebSocket connections from one IP address (IPv6: per /64), whole server. 64 lets a class \
         or a mobile operator's shared address (carrier-grade NAT) play; one live connection per account still \
         applies, and IP_MAX_CONNECTIONS bounds every connection of an address before TLS.",
    )
    .default("64")
    .min(1),
    key(
        "MAX_PENDING_HANDSHAKES",
        Limits,
        Int,
        "TLS handshakes in progress, whole server (TLS_MODE=native). Empty (the default) = 128 x WORKERS. A new \
         connection takes a slot once the first record of its ClientHello has arrived; it has 3 s for that and \
         holds no slot meanwhile. A connection beyond this cap, or beyond MAX_PENDING_HANDSHAKES_PER_IP for its \
         address group, is closed before any TLS work and the client retries later, so a reconnection storm is \
         served in turn instead of every handshake slowing down together. The server also sheds load, letting \
         at most half this number of new TLS connections per second through, for up to 5 s after it refused a \
         WebSocket upgrade because MAX_CONNECTIONS and its reserve are in use, or while it holds 1.2 times \
         MAX_CONNECTIONS. It does not shed at MAX_CONNECTIONS itself, so that a player coming back to a game in \
         progress does not compete with newcomers for that rate: each newcomer then completes the handshake \
         and gets ServerFull at Hello.",
    )
    .range(2, 100_000),
    key(
        "MAX_PENDING_HANDSHAKES_PER_IP",
        Limits,
        Int,
        "TLS handshakes in progress for one address group: an IPv4 address or an IPv6 /48 (TLS_MODE=native), \
         whole server. Empty (the default) = MAX_PENDING_HANDSHAKES / 32 with a floor of 2, but always below \
         MAX_PENDING_HANDSHAKES (4 x WORKERS with the default MAX_PENDING_HANDSHAKES; check-config prints the \
         value in use). A value you set must be lower than MAX_PENDING_HANDSHAKES, so that a few hosts cannot \
         hold every handshake slot. The server also keeps at most 4 times this number of connections of one \
         group waiting for their ClientHello (and 16 times MAX_PENDING_HANDSHAKES in total). Raise it when many \
         players share one public address (a school or company network); a handshake takes a fraction of a \
         second, so a small value still serves many players.",
    )
    .range(1, 99_999),
    key("WS_MSG_RATE", Limits, Int, "Messages per second a client may send (sustained).").default("20").min(1),
    key("WS_MSG_BURST", Limits, Int, "Message burst a client may send.").default("40").min(1),
    key(
        "WS_SEND_BUFFER_LIMIT",
        Limits,
        Int,
        "Bytes queued for a client that does not read; beyond it the connection is closed (the client \
         reconnects and resynchronises).",
    )
    .default("262144")
    .min(4096),
    key("WS_HELLO_TIMEOUT_MS", Limits, Int, "Time a new connection has to authenticate.")
        .default("10000")
        .range(1000, 600_000),
    key(
        "HEARTBEAT_INTERVAL_MS",
        Limits,
        Int,
        "Server ping interval: each connection gets a ping every half interval to one interval (it also \
         measures each player's latency). The game client sends a Ping of its own after 1.5 times this with \
         nothing received (at least 7.5 s, at most 90 s), and considers the connection dead after twice this \
         (at least 10 s, at most 120 s).",
    )
    .default("10000")
    .min(1000),
    key("HEARTBEAT_TIMEOUT_MS", Limits, Int, "A connection silent for this long is considered dead.")
        .default("30000")
        .min(3000),
    key(
        "CLIENT_PING_INTERVAL_MS",
        Limits,
        Int,
        "Interval of the game client's own Ping, announced in Welcome (the client measures its round trip for \
         the ping indicator and its estimate of the server clock with it). Lower is a more reactive ping \
         indicator but costs more server CPU for every connected player (docs/SIZING.md). After each \
         connection the client sends a few quick pings anyway.",
    )
    .default("10000")
    .range(1000, 60_000),
    key(
        "GESTURE_RATE",
        Limits,
        Int,
        "Live gestures (the player's head, the piece in hand and where it is aimed) a client may send per \
         second, sustained, announced in Welcome. The server relays each one to the opponent as it is and \
         never stores it; it costs server CPU for every player in a game (docs/SIZING.md). A client beyond it \
         has its gestures dropped silently, and only a gross excess closes the connection as a flood. 0 turns \
         the relay off (the clients then send none).",
    )
    .default("4")
    .range(0, 60),
    key(
        "GESTURE_BURST",
        Limits,
        Int,
        "Gestures a client may send in a burst above GESTURE_RATE (the size of its own token bucket, apart \
         from WS_MSG_RATE: gestures never delay or rate-limit moves).",
    )
    .default("8")
    .range(1, 120),
    key(
        "HTTP_BODY_LIMIT",
        Limits,
        Int,
        "Largest API request body in bytes, except POST /api/v1/gif, which has its own fixed limit of 135,168 \
         bytes (a PGN of up to 64 KiB as a JSON string, escapes included).",
    )
    .default("16384")
    .min(1024),
    key(
        "AUTH_RATE_PER_IP",
        Limits,
        Int,
        "Login / register / reset attempts per 10 minutes from one IP address (one IPv6 /64).",
    )
    .default("20")
    .min(1),
    key(
        "AUTH_RATE_PER_PREFIX",
        Limits,
        Int,
        "The AUTH_RATE_PER_IP limits (login / register / reset attempts, and account changes that ask for the \
         password), per 10 minutes for one IPv6 /48 as a whole, on top of the limit of each of its /64 \
         networks: a /48 holds 65536 of them, and one customer often gets a /56 or a /48. 0 means 5 x \
         AUTH_RATE_PER_IP. Raise it for a site that brings many players at once over one IPv6 prefix (a \
         campus, a club event). IPv4 addresses are only limited one by one.",
    )
    .default("0")
    .min(0),
    key(
        "AUTH_FAILURES_PER_ACCOUNT",
        Limits,
        Int,
        "Failed logins on one account before each further attempt is delayed exponentially (up to 15 minutes).",
    )
    .default("5")
    .min(1),
    key(
        "AUTH_REGISTER_PER_HOUR",
        Limits,
        Int,
        "Registrations per hour from one IPv4 address or IPv6 /64 (3 times that per IPv6 /48), whole server, \
         on top of AUTH_RATE_PER_IP. Raise it for a session where a class creates its accounts together.",
    )
    .default("10")
    .min(1),
    key(
        "AUTH_MAIL_PER_HOUR",
        Limits,
        Int,
        "Confirmation e-mails asked again (POST /auth/verify-email/resend) per hour from one IPv4 address or \
         IPv6 /64 (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP and of the one e-mail per \
         address every 5 minutes. Password reset e-mails have their own, stricter limits \
         (AUTH_FORGOT_PER_HOUR, AUTH_FORGOT_PER_DAY).",
    )
    .default("10")
    .min(1),
    key(
        "AUTH_FORGOT_PER_HOUR",
        Limits,
        Int,
        "Password reset e-mails asked (POST /auth/password/forgot) per hour from one IPv4 address or IPv6 /64 \
         (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP and of the one e-mail per address \
         every 5 minutes. A refusal is a 429, which says nothing about the address; an accepted request \
         answers 202 whether the address has an account or not.",
    )
    .default("3")
    .min(1),
    key(
        "AUTH_FORGOT_PER_DAY",
        Limits,
        Int,
        "The same as AUTH_FORGOT_PER_HOUR per 24 hours (3 times that per /48). At least AUTH_FORGOT_PER_HOUR.",
    )
    .default("10")
    .min(1),
    key(
        "AUTH_RESET_PER_HOUR",
        Limits,
        Int,
        "New passwords sent with a reset link (POST /auth/password/reset and the /reset-password page) per hour \
         from one IPv4 address or IPv6 /64 (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP: \
         each one hashes a password.",
    )
    .default("10")
    .min(1),
    key(
        "AUTH_MFA_PER_ACCOUNT",
        Limits,
        Int,
        "Second-factor codes (authenticator or recovery codes) tried per 15 minutes for one account, whole \
         server, from any address, at sign-in and in account changes; then 429 too_many_attempts before the \
         code is checked (a recovery code is not spent). It bounds a code guesser who knows the password, \
         whatever the number of addresses.",
    )
    .default("10")
    .min(1),
    key(
        "AUTH_REAUTH_PER_USER",
        Limits,
        Int,
        "Account changes that ask for the password or a code (password, two-step verification, e-mail, data \
         export, deletion) per 10 minutes for one account, whole server, from any address, on top of the \
         per-address limit AUTH_RATE_PER_IP: a stolen session used from many addresses cannot guess the \
         password faster.",
    )
    .default("10")
    .min(1),
    key(
        "USER_RATE_PER_MIN",
        Limits,
        Int,
        "API requests per minute of one signed-in account (every request with a valid session token), all \
         endpoints together, whatever its address, whole server, with a burst of half a minute; beyond it 429 \
         rate_limited with Retry-After. The game's busiest use, paging through the history, is about one \
         request per second.",
    )
    .default("120")
    .min(1),
    key(
        "CHALLENGE_UNPLAYED_PER_MIN",
        Limits,
        Int,
        "Direct challenges of one player that may end withdrawn or declined within a minute (each one popped \
         up on its target's screen, and no game came of it); the player's next direct challenge is then \
         refused with ChallengeLimit until the minute has passed, so that create/cancel cycles cannot flood a \
         player with challenges. Accepted challenges and private games do not count.",
    )
    .default("5")
    .min(1),
    key(
        "PRIVATE_CODE_FAILURES_PER_MIN",
        Limits,
        Int,
        "Wrong private game codes one player may try within a minute; ChallengeJoinCode is then refused with \
         RateLimited, even for a right code, until the minute has passed, so that the codes of other players' \
         private games cannot be guessed.",
    )
    .default("10")
    .min(1),
    key(
        "POW_REGISTER_BITS",
        Limits,
        Int,
        "Proof-of-work difficulty (leading zero bits of SHA-256) required to register; 0 disables it.",
    )
    .default("18")
    .range(0, 26),
    key(
        "POW_LOGIN_BITS",
        Limits,
        Int,
        "Proof-of-work difficulty required to log in while the server sees a credential-stuffing wave; 0 \
         disables it.",
    )
    .default("18")
    .range(0, 26),
    key(
        "POW_LOGIN_TRIGGER_PER_MIN",
        Limits,
        Int,
        "Failed logins per minute (whole server) that turn on the login proof-of-work. Every failed login costs \
         a password hash (about 0.5 s of CPU), so 30 per minute already keeps a quarter of a core busy, and a \
         few hundred would need several cores: with PASSWORD_HASH_CONCURRENCY at one per core, a small server \
         could never reach such a trigger. Raise it only on a large server where honest typos alone come near \
         it.",
    )
    .default("30")
    .min(1),
    key(
        "PASSWORD_HASH_CONCURRENCY",
        Limits,
        Int,
        "Password hashes and verifications (login, registration, password change and reset, account changes \
         that ask for the password) that the server runs at once, each on a thread of its own. Each costs \
         about 0.5 s of CPU and 64-128 MiB. Empty (the default) = WORKERS. check-config warns when it is above \
         the number of CPU cores, as the hashes would then slow the games down.",
    )
    .range(1, 64),
    key(
        "PASSWORD_HASH_QUEUE_MAX",
        Limits,
        Int,
        "Password hashes that may wait for a free slot, whole server; one more is refused at once with 503 \
         server_busy and a Retry-After of 5 to 15 s (0: no waiting at all). Empty (the default) = 32 x \
         WORKERS. Once half of them wait, one client (an IPv4 address, or an IPv6 /48) may have at most \
         PASSWORD_HASH_WAITERS_PER_SOURCE of them waiting; its next one is refused with 429 rate_limited.",
    )
    .min(0),
    key(
        "PASSWORD_HASH_WAITERS_PER_SOURCE",
        Limits,
        Int,
        "Password hashes one client (an IPv4 address, or an IPv6 /48) may have waiting once \
         PASSWORD_HASH_QUEUE_MAX is at least half full, whole server; its next request is then refused with \
         429 rate_limited and a Retry-After of 5 to 15 s, and that refused attempt does not count against \
         AUTH_RATE_PER_IP. Empty (the default) = 2 x WORKERS. While less than half of the queue waits, one \
         client may queue more, so that players who log in together behind one address (a school or a company \
         network) are served when the server is not busy, and, as long as PASSWORD_HASH_WAITERS_PER_SOURCE is \
         at most half of PASSWORD_HASH_QUEUE_MAX, one client never holds more than half of the queue. Raise it \
         for such a site if its players log in while the server is busy, together with \
         MAX_PENDING_HANDSHAKES_PER_IP and AUTH_RATE_PER_IP.",
    )
    .min(1),
    key(
        "PASSWORD_HASH_QUEUE_TIMEOUT_MS",
        Limits,
        Int,
        "Longest wait for a password hash slot, for all the hashes of one request together (a password change \
         hashes twice); the request is then refused with 503 server_busy. At most 13000: the game gives up \
         after 15 s, and the hash itself takes a second or two, so that the player sees the \"busy\" answer \
         rather than a timeout.",
    )
    .default("10000")
    .range(100, 13_000),
    // ---- Games ------------------------------------------------------------------------------
    key(
        "RATED_CATEGORIES",
        Games,
        List,
        "Official time controls (minutes+increment seconds). Each one has its own Elo rating; any other time \
         control is \"Custom\" and never rated.",
    )
    .default("1+0,3+0,3+2,5+0,5+3,10+0,10+5,15+10,30+0,30+20,90+30"),
    key(
        "ALLOW_CUSTOM_TIME_CONTROLS",
        Games,
        Bool,
        "Direct challenges and private games may use custom (unrated) time controls.",
    )
    .default("true"),
    key(
        "FIRST_MOVE_TIMEOUT_MS",
        Games,
        Int,
        "A player who does not make their first move in time: the game is aborted (no rating change).",
    )
    .default("30000")
    .min(5000),
    key("RECONNECT_GRACE_MIN_MS", Games, Int, "Shortest time a disconnected player has to come back.")
        .default("15000")
        .min(5000),
    key(
        "RECONNECT_GRACE_MAX_MS",
        Games,
        Int,
        "Longest time a disconnected player has to come back (the grace is 10% of the base time within these \
         bounds).",
    )
    .default("60000")
    .min(5000),
    key(
        "RECOVERY_GRACE_MS",
        Games,
        Int,
        "Time both players of a game restored from the journal after a restart or a crash have to come back \
         (or the normal grace when it is longer): the server, not the players, broke the connection, and \
         every client reconnects at once. The clock of the side to move stays stopped until that player is \
         back, for RECOVERY_CLOCK_HOLD_MS at most.",
    )
    .default("90000")
    .range(15_000, 3_600_000),
    key(
        "RECOVERY_CLOCK_HOLD_MS",
        Games,
        Int,
        "After a restart or a crash, the clock (or the first-move timer) of the side to move of a restored game \
         does not run until that player is back, and runs again after this long even if they are still away. \
         When it is not set, it is 20000, or RECOVERY_GRACE_MS - 1 when RECOVERY_GRACE_MS is 20000 or less; a \
         value you set must be lower than RECOVERY_GRACE_MS. It bounds the free thinking time a player could \
         get by staying away on purpose; 0 restarts the clock at the recovery.",
    )
    .default("20000")
    .range(0, 3_599_999),
    key("LAG_COMP_MAX_MS", Games, Int, "Largest network lag given back on one move.").default("1000").range(0, 5000),
    key("LAG_QUOTA_INITIAL_MS", Games, Int, "Lag compensation budget of each player at the start of a game.")
        .default("2000")
        .min(0),
    key("LAG_QUOTA_GAIN_MS", Games, Int, "Lag compensation budget regained at every move.").default("100").min(0),
    key("LAG_QUOTA_MAX_MS", Games, Int, "Largest lag compensation budget.").default("3000").min(0),
    key(
        "GAME_STALL_MIN_MS",
        Games,
        Int,
        "A game shard that could not run for longer than this (CPU steal, a busy or blocked runtime thread, a \
         paused virtual machine) counts as stalled: the game requests that waited meanwhile (moves, \
         resignations, draw offers and answers, claims, aborts, Resyncs and the closing of a connection) are \
         handled before its timers, as if they had arrived when the stall began, so that a flag or a \
         first-move timeout that fell during the stall does not overtake them; a first-move timeout that fell \
         during it records no no-show against the player. Shorter pauses change nothing.",
    )
    .default("30")
    .range(5, 1000),
    key(
        "GAME_STALL_CREDIT_MAX_MS",
        Games,
        Int,
        "Longest stall of a game shard that is not charged to the players (see GAME_STALL_MIN_MS): a message \
         handled after a longer stall counts as arrived this long before it was read. It is also the most a \
         player can gain from one stall. 0 charges every stall to the side to move, as a server without this \
         protection would.",
    )
    .default("5000")
    .range(0, 60_000),
    key(
        "AUTO_PRESS_CLOCK",
        Games,
        Bool,
        "The players' robots press the clock by themselves once a move is on the board. When false, a client \
         sends its move only when its player presses the clock, so the mover's clock runs until then. Decided \
         when a game is created (a rematch keeps the value of the game it follows) and kept by the game, \
         restarts included.",
    )
    .default("true"),
    key("DRAW_OFFERS_PER_GAME", Games, Int, "Draw offers one player may make in a game.").default("3").min(0),
    key("CHALLENGE_TTL_MS", Games, Int, "A direct challenge expires after this long.").default("60000").min(5000),
    key("PRIVATE_GAME_TTL_MS", Games, Int, "A private game code expires after this long.")
        .default("900000")
        .min(60_000),
    // ---- Matchmaking and ratings ------------------------------------------------------------
    key(
        "INITIAL_RATING",
        Matchmaking,
        Int,
        "Working rating of a new player in every category: shown and used for pairing until their first \
         rating, which FIDE's rules compute after five counted games (a game lost before the loser's first \
         draw or win counts for neither player: docs/DESIGN.md, ratings), and what an unrated opponent counts \
         for in those games.",
    )
    .default("1500")
    .range(100, 3000),
    key(
        "PROVISIONAL_GAMES",
        Matchmaking,
        Int,
        "Counted games in a category (those that entered the rating: the five of the unrated phase, then the \
         games against rated opponents) during which the rating is provisional: K = 40, shown with \"?\" and \
         off the leaderboard (an unrated player is always provisional). FIDE uses 30.",
    )
    .default("30")
    .range(0, 100),
    key("MATCH_TICK_MS", Matchmaking, Int, "Interval between pairing rounds.").default("250").range(50, 5000),
    key("MATCH_WINDOW_START", Matchmaking, Int, "Largest rating difference accepted right after joining the queue.")
        .default("100")
        .min(0),
    key("MATCH_WINDOW_STEP", Matchmaking, Int, "Widening of the window at each step.").default("50").min(0),
    key("MATCH_WINDOW_STEP_MS", Matchmaking, Int, "Time between two widenings.").default("5000").min(100),
    key("MATCH_WINDOW_MAX", Matchmaking, Int, "Widest window (reached after about a minute with the defaults).")
        .default("500")
        .min(0),
    key(
        "MATCH_PROVISIONAL_BONUS",
        Matchmaking,
        Int,
        "Extra window for a provisional rating (its value is still uncertain).",
    )
    .default("150")
    .min(0),
    key(
        "MATCH_REPEAT_LIMIT",
        Matchmaking,
        Int,
        "Rated games two players may play together within MATCH_REPEAT_WINDOW_MS, whatever made them (queue, \
         direct challenge, private game, rematch); beyond it the matchmaker no longer pairs them, and their \
         rated challenges, private games and rematches are refused (limits rating manipulation between \
         friends). Unrated games stay free. The counts are kept in memory: a restart forgets them.",
    )
    .default("3")
    .min(1),
    key("MATCH_REPEAT_WINDOW_MS", Matchmaking, Int, "See MATCH_REPEAT_LIMIT.").default("3600000").min(60_000),
    key(
        "CONDUCT_ABANDON_LIMIT",
        Matchmaking,
        Int,
        "Abandoned / aborted / no-show games in 24 hours before rated matchmaking is paused for the player (15 \
         min, then 1 h, then 6 h).",
    )
    .default("3")
    .min(1),
    // ---- Anti-cheat and sanctions -----------------------------------------------------------
    key(
        "AUTO_SANCTION_CERTAIN_CHEATS",
        Anticheat,
        Bool,
        "A technically certain cheat (forged protocol, illegal move in a synchronised position, playing out of \
         turn) loses the game, disconnects the player and bans them for BAN_DURATION_HOURS.",
    )
    .default("true"),
    key("BAN_DURATION_HOURS", Anticheat, Int, "Length of an automatic ban.").default("24").range(1, 87_600),
    key(
        "RATING_REFUND_DAYS",
        Anticheat,
        Int,
        "When a player is banned as a cheater (a certain cheat, or a moderator's integrity confirm), each \
         opponent who lost rating points to them in a rated game that ended within this many days before the \
         ban gets those points back on their current rating (docs/ANTICHEAT.md, rating refunds). A game still \
         in progress at the ban (or not recorded yet) is refunded when it is recorded, while that ban lasts \
         (not after an integrity confirm --no-refund; a user ban refunds nothing). 0: no refunds unless a \
         moderator asks for them (scacelith-server admin refunds apply).",
    )
    .default("60")
    .range(0, 3650),
    key(
        "ANALYSIS_ENGINE_PATH",
        Anticheat,
        Path,
        "UCI engine used to analyse rated games after they end: the official Stockfish 19 release binary for \
         Linux x86-64, the engine the anti-cheat is calibrated for (docs/ANTICHEAT.md, section 3). Empty: \
         engine-based statistics are disabled (timing and reports still count). Another engine or network \
         restarts the statistics (they are kept per analysis profile).",
    )
    .default(""),
    key(
        "ANALYSIS_WORKERS",
        Anticheat,
        Int,
        "Engine processes analysing games (each uses one core, at low priority). Stockfish 19 engines share \
         one copy of their network: about 70 MB per engine after the first, 180 MB when /tmp is not writable \
         (docs/SIZING.md).",
    )
    .default("1")
    .range(0, 64),
    key(
        "ANALYSIS_DEPTH_FAST",
        Anticheat,
        Int,
        "Shallow analysis depth (a weak engine's choice). Changing it restarts the statistics, like \
         ANALYSIS_DEPTH_DEEP.",
    )
    .default("9")
    .range(4, 30),
    key(
        "ANALYSIS_DEPTH_DEEP",
        Anticheat,
        Int,
        "Deep analysis depth (a strong engine's choice). On a VPS vCore (AVX2), Stockfish 19 at 9/15 costs 14 % \
         less than Stockfish 16 at the former 10/18, and flags engine users earlier than at 9/14, 9/16 or 10/18 \
         in most cases (docs/ANTICHEAT.md, calibration). Changing it restarts the statistics (they are kept per \
         analysis profile: engine, network, depths, hash).",
    )
    .default("15")
    .range(6, 40),
    key("ANALYSIS_MIN_PLIES", Anticheat, Int, "Shorter games are not analysed.").default("30").min(10),
    key("REPORTS_PER_DAY", Anticheat, Int, "Reports one player may file per day.").default("5").min(1),
    key(
        "ANALYSIS_HASH_MB",
        Anticheat,
        Int,
        "Transposition table of each analysis engine, in MB. Changing it restarts the statistics, like \
         ANALYSIS_DEPTH_DEEP.",
    )
    .default("32")
    .range(1, 4096),
    key(
        "ANALYSIS_POSITION_TIMEOUT_MS",
        Anticheat,
        Int,
        "Longest search of one position; an engine that exceeds it is restarted and the game is marked failed.",
    )
    .default("120000")
    .range(1000, 3_600_000),
    key(
        "ANALYSIS_POLL_MS",
        Anticheat,
        Int,
        "Interval at which an idle analysis engine looks for new games to analyse.",
    )
    .default("5000")
    .range(100, 3_600_000),
    key(
        "ANALYSIS_QUEUE_MAX",
        Anticheat,
        Int,
        "Most ordinary games waiting for engine analysis: while this many wait, a newly finished ordinary game \
         is not queued (the engines could not catch up anyway). Games with a report, a suspicion signal (at \
         most 20 waiting per player) or a moderator request are queued anyway and mostly analysed first; one \
         engine claim in four still goes to the oldest ordinary game. 0 analyses only those (and then no game \
         feeds the population statistics). At most 100000: the waiting ordinary games are counted in every \
         commit of finished games, under the database write lock.",
    )
    .default("5000")
    .range(0, 100_000),
    key(
        "ANALYSIS_SAMPLE_RATE",
        Anticheat,
        Number,
        "Share of the ordinary rated games queued for analysis (0 to 1, drawn at random when the game ends). \
         Lower it when the engine cannot keep up with the games played.",
    )
    .default("1")
    .range(0, 1),
    // ---- Animated GIFs of games -------------------------------------------------------------
    key(
        "GIF_ENABLED",
        Gif,
        Bool,
        "Animated GIFs of games for signed-in players: GET /api/v1/games/:id/gif (a game of this server) and \
         POST /api/v1/gif (any game, as a PGN). Rendered on threads of their own at the lowest CPU priority, \
         never on the threads of the games. false: both endpoints answer 404 gif_disabled.",
    )
    .default("true"),
    key(
        "GIF_THREADS",
        Gif,
        Int,
        "GIF renders at the same time, whole server, each on a thread of its own at the lowest CPU priority (on \
         Linux: it only takes the CPU the games leave). Empty (the default) = WORKERS. A render takes one core \
         (measured on a 2.1 GHz Xeon: about 45 ms for a 40-move game at the medium size, about 0.7 s for a game \
         of GIF_MAX_PLIES at the large size; docs/SIZING.md), and a thread 40-50 MiB of memory while it lives \
         (up to about 125 MiB after many of the longest games at the large size). The threads start on demand \
         and stop after a minute without work.",
    )
    .range(1, 64),
    key(
        "GIF_QUEUE_MAX",
        Gif,
        Int,
        "Renders that may wait for a free thread, whole server; one more is refused at once with 503 \
         server_busy and Retry-After, and the render quotas it took are given back. Empty (the default) = 4 x \
         WORKERS.",
    )
    .range(0, 1024),
    key(
        "GIF_QUEUE_TIMEOUT_MS",
        Gif,
        Int,
        "Longest wait of a render for a free thread; then 503 server_busy (the render quotas are given back).",
    )
    .default("10000")
    .range(100, 60_000),
    key(
        "GIF_RENDER_TIMEOUT_MS",
        Gif,
        Int,
        "Longest render: the thread is stopped (a new one starts with the next render) and the request answers \
         500.",
    )
    .default("30000")
    .range(1000, 120_000),
    key(
        "GIF_MAX_PLIES",
        Gif,
        Int,
        "Longest game, in half-moves, a GIF shows; a longer one answers 422 game_too_long. 600 plies last 5 \
         minutes at the default speed (0.5 s per move).",
    )
    .default("600")
    .range(1, 1200),
    key(
        "GIF_CACHE_MB",
        Gif,
        Int,
        "Memory of the cache of rendered GIFs, whole server (the least recently used goes first; a medium GIF \
         of 80 plies is about 200 KiB). A GIF served from the cache costs no render quota. 0 disables the \
         cache. Empty (the default) = 32 x WORKERS.",
    )
    .range(0, 16_384),
    key(
        "GIF_USER_RENDERS_PER_MIN",
        Gif,
        Int,
        "GIF renders per minute of one account, whole server (a GIF served from the cache does not count); \
         beyond it 429 rate_limited with Retry-After.",
    )
    .default("4")
    .min(1),
    key(
        "GIF_USER_RENDERS_PER_HOUR",
        Gif,
        Int,
        "GIF renders per hour of one account, whole server. At least GIF_USER_RENDERS_PER_MIN.",
    )
    .default("30")
    .min(1),
    key(
        "GIF_IP_RENDERS_PER_MIN",
        Gif,
        Int,
        "GIF renders per minute from one IPv4 address or IPv6 /64 (3 times that per IPv6 /48), all accounts \
         together, whole server: a background ceiling for many accounts behind one address.",
    )
    .default("12")
    .min(1),
    key(
        "GIF_IP_RENDERS_PER_HOUR",
        Gif,
        Int,
        "GIF renders per hour from one IPv4 address or IPv6 /64 (3 times that per /48), all accounts together, \
         whole server. At least GIF_IP_RENDERS_PER_MIN.",
    )
    .default("120")
    .min(1),
    // ---- Observability ----------------------------------------------------------------------
    key(
        "METRICS_PORT",
        Observability,
        Port,
        "Prometheus metrics and health endpoint (plain HTTP; 0 disables it).",
    )
    .default("9464"),
    key("METRICS_BIND", Observability, Text, "Keep it private: 127.0.0.1 or an internal address.")
        .default("127.0.0.1"),
    key(
        "METRICS_TOKEN",
        Observability,
        SecretText,
        "Optional bearer token required to read the metrics: /metrics then needs the header \"Authorization: \
         Bearer <token>\" with this exact text (no spaces).",
    )
    .default(""),
    key("LOG_LEVEL", Observability, Enum(&["debug", "info", "warn", "error"]), "Log verbosity.").default("info"),
    key(
        "LOG_FORMAT",
        Observability,
        Enum(&["json", "pretty"]),
        "JSON lines (for log collectors) or readable text, on stdout (stderr for migrate). When that stream is \
         the journal (JOURNAL_STREAM, set by systemd), each line starts with its syslog priority (<7> debug, \
         <6> info, <5> security, <4> warn, <3> error).",
    )
    .default("json"),
    key(
        "LOG_IP",
        Observability,
        Enum(&["truncated", "full", "hashed"]),
        "How client addresses appear in the logs: truncated (IPv4 /24, IPv6 /48), full, or hashed (keyed HMAC, \
         rotated daily).",
    )
    .default("truncated"),
    key(
        "RETENTION_SECURITY_DAYS",
        Observability,
        Int,
        "Security events (failed logins, anomalies without sanction) are deleted after this many days.",
    )
    .default("90")
    .min(1),
    key(
        "RETENTION_IP_DAYS",
        Observability,
        Int,
        "Stored IP addresses (sessions, security events) are erased after this many days.",
    )
    .default("30")
    .min(1),
    key(
        "RETENTION_INTERVAL_MS",
        Observability,
        Int,
        "Interval of the retention purge (expired sessions, tokens and pending signups, old security events, \
         anomalies, conduct events and failed analysis jobs, IP erasure). The first run starts about a minute \
         after the server starts. At most 2147483647 (about 24.8 days).",
    )
    .default("3600000")
    .range(60_000, 2_147_483_647),
];
