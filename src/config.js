// Server configuration: every setting an administrator can change, read from the environment
// and from an optional .env file (KEY=value lines). This table is the single source of truth:
// `.env.example` and the README's configuration table are generated from it
// (`npm run gen:env`).
//
// Secrets are never given defaults. A secret can be passed directly (FOO=...) or, preferably,
// through a file (FOO_FILE=/run/secrets/foo), which keeps it out of the process environment
// listing and of shell history.
//
// Modules receive the frozen object returned by loadConfig(); they never read process.env.

import fs from 'node:fs';
import os from 'node:os';
import path from 'node:path';

const KEYS = [];
function key(name, spec) { KEYS.push({ name, ...spec }); }

// ---- Server identity and network ----------------------------------------------------------------
key('SERVER_NAME', { section: 'server', type: 'string', default: 'Scacelith Community Server', max: 64,
    desc: 'Name shown to players (menus, scoresheet "Event").' });
key('SERVER_PUBLIC_HOST', { section: 'server', type: 'string', default: 'localhost',
    desc: 'Public DNS name of the server, used in e-mail links and the Google SSO redirect URI.' });
key('SERVER_MOTD', { section: 'server', type: 'string', default: '', max: 200,
    desc: 'Short message of the day shown in the online menu.' });
key('BIND_ADDRESS', { section: 'server', type: 'string', default: '0.0.0.0', desc: 'Address the API and WebSocket listeners bind to.' });
key('API_PORT', { section: 'server', type: 'port', default: 44664,
    desc: 'HTTPS API port (TCP). 44664 is the port of the official server; any free port works for a community server.' });
key('WS_PORT', { section: 'server', type: 'port',
    desc: 'WSS (game WebSocket) port. Empty (the default) = the same port as API_PORT: one TLS listener serves the API under /api/v1 and the WebSocket upgrade on /ws. Set another port to split them.' });
key('PUBLIC_API_PORT', { section: 'server', type: 'port', default: 0,
    desc: 'API port as seen by clients when a proxy/NAT maps ports (0 = API_PORT).' });
key('PUBLIC_WS_PORT', { section: 'server', type: 'port', default: 0,
    desc: 'WSS port as seen by clients (0 = WS_PORT).' });
key('WORKERS', { section: 'server', type: 'string', default: 'auto',
    desc: 'Worker processes (shards) handling connections and games: a number, or "auto" (one per CPU core, at most 16).' });
key('SHARD_BASE', { section: 'server', type: 'int', default: 0, min: 0, max: 56,
    desc: 'First shard number of this instance (multi-instance deployments give each instance its own range).' });
key('INSTANCE_ID', { section: 'server', type: 'string', default: '', desc: 'Free label of this instance in logs and metrics (default: host name).' });
key('WS_ALLOWED_ORIGINS', { section: 'server', type: 'list', default: '',
    desc: 'Origin header values allowed to open the game WebSocket (e.g. https://play.example.org). The game client sends no Origin; browsers always send one, so they are refused unless listed here.' });
key('SHUTDOWN_GRACE_MS', { section: 'server', type: 'int', default: 3000, min: 0, max: 120000,
    desc: 'On SIGTERM/SIGINT players are warned (ServerShutdown notice) this long before their connections close. Games in progress survive the restart (journal).' });
key('LISTEN_REUSE_PORT', { section: 'server', type: 'bool', default: false,
    desc: 'Linux: every worker binds its own listening socket (SO_REUSEPORT) and the kernel spreads new connections, instead of the primary accepting them and handing them out round-robin. Ignored on other systems.' });
key('LISTEN_BACKLOG', { section: 'server', type: 'int', default: 2048, min: 128, max: 65535,
    desc: 'Length of the kernel queue of new connections not yet accepted (listen backlog), which absorbs reconnection bursts. The kernel caps it at net.core.somaxconn (Linux), so raise that sysctl as well (README, kernel settings).' });
key('SHARD_OVERLOAD_LAG_MS', { section: 'server', type: 'int', default: 250, min: 5, max: 5000,
    desc: 'Event-loop delay (p99, ms) above which a worker counts as overloaded: new games are then hosted by the least loaded worker.' });

// ---- TLS -----------------------------------------------------------------------------------------
key('TLS_MODE', { section: 'tls', type: 'enum', values: ['native', 'proxy', 'off'], default: 'native',
    desc: 'native: this server terminates TLS with TLS_CERT_FILE/TLS_KEY_FILE. proxy: a reverse proxy (nginx, haproxy, caddy) terminates TLS and forwards plain HTTP/WebSocket to this server on a private address. off: plain text, refused unless ALLOW_INSECURE_DEV=1 (local development only).' });
key('TLS_CERT_FILE', { section: 'tls', type: 'path', default: '', desc: 'PEM certificate chain (fullchain). Reloaded on SIGHUP and when the file changes.' });
key('TLS_KEY_FILE', { section: 'tls', type: 'path', default: '', secretFile: true, desc: 'PEM private key. Never commit it.' });
key('TLS_MIN_VERSION', { section: 'tls', type: 'enum', values: ['TLSv1.2', 'TLSv1.3'], default: 'TLSv1.2', desc: 'Oldest TLS version accepted.' });
key('TRUSTED_PROXIES', { section: 'tls', type: 'list', default: '127.0.0.1,::1',
    desc: 'With TLS_MODE=proxy: addresses whose X-Forwarded-For / X-Forwarded-Proto headers are trusted.' });
key('ALLOW_INSECURE_DEV', { section: 'tls', type: 'bool', default: false, desc: 'Allows TLS_MODE=off. Development only; never on a public server.' });

// ---- Storage -------------------------------------------------------------------------------------
key('DATA_DIR', { section: 'storage', type: 'path', default: './data', desc: 'Directory for the database, the game journal and runtime sockets.' });
key('DB_PATH', { section: 'storage', type: 'path', default: '', desc: 'SQLite database file (default: DATA_DIR/scacelith.db).' });
key('JOURNAL_DIR', { section: 'storage', type: 'path', default: '', desc: 'Append-only journal of the games in progress, replayed after a crash (default: DATA_DIR/journal).' });
key('JOURNAL_FLUSH_MS', { section: 'storage', type: 'int', default: 50, min: 5, max: 1000,
    desc: 'Longest time a game event waits in memory before being written to the journal (group commit).' });
key('JOURNAL_FSYNC', { section: 'storage', type: 'bool', default: true, desc: 'fsync the journal at every flush (survives power loss, not only process crashes).' });
key('JOURNAL_COMPACT_SEGMENTS', { section: 'storage', type: 'int', default: 4, min: 1, max: 1000,
    desc: 'Journal compaction: once a shard\'s journal has moved this many 16 MB segments past the oldest record a game still needs (its first record, or its latest snapshot), the game, running or waiting for its database commit, is written again as one snapshot record and the older segments can be deleted. A shard\'s journal then stays around (this + 1) x 16 MB however long the games last. Lower values write more snapshots; higher ones keep more on disk and lengthen the replay after a restart.' });
key('DB_COMMIT_MS', { section: 'storage', type: 'int', default: 50, min: 1, max: 2000,
    desc: 'Finished games are committed to the database in batches, at most this long after they end.' });
key('DB_CACHE_MB', { section: 'storage', type: 'int', default: 64, min: 2, max: 4096,
    desc: 'SQLite page cache of each server process (the primary and every worker), in megabytes.' });
key('DB_MMAP_MB', { section: 'storage', type: 'int', default: 256, min: 0, max: 65536,
    desc: 'Part of the database file read through memory mapping, in megabytes (0 disables it).' });

// ---- Secrets ---------------------------------------------------------------------------------------
key('SERVER_SECRET', { section: 'secrets', type: 'secret', required: true, minBytes: 32,
    desc: 'Master secret (at least 32 random bytes, hex or base64). Keys for proof-of-work challenges, recovery-code hashing, OAuth state and MFA secret encryption are derived from it with HKDF. Generate one with: node -e "console.log(require(\'crypto\').randomBytes(48).toString(\'base64\'))"' });
key('MFA_ENCRYPTION_KEY', { section: 'secrets', type: 'secret', default: '', minBytes: 32,
    desc: 'Optional separate key (32 bytes) encrypting TOTP secrets at rest. Default: derived from SERVER_SECRET. Changing it makes existing TOTP enrolments unreadable.' });

// ---- Accounts ------------------------------------------------------------------------------------
key('REGISTRATION', { section: 'accounts', type: 'enum', values: ['open', 'closed'], default: 'open', desc: 'Whether new accounts can be created from the game.' });
key('REQUIRE_EMAIL_VERIFICATION', { section: 'accounts', type: 'bool', default: true,
    desc: 'Accounts must confirm their e-mail address before playing online.' });
key('USERNAME_MIN', { section: 'accounts', type: 'int', default: 3, min: 2, max: 24, desc: 'Shortest username.' });
key('USERNAME_MAX', { section: 'accounts', type: 'int', default: 20, min: 3, max: 24, desc: 'Longest username (the scoresheet has room for 24 characters).' });
key('PASSWORD_MIN_LENGTH', { section: 'accounts', type: 'int', default: 10, min: 8, max: 64, desc: 'Shortest password.' });
key('SESSION_IDLE_DAYS', { section: 'accounts', type: 'int', default: 30, min: 1, max: 365, desc: 'A session unused for this long expires.' });
key('SESSION_MAX_DAYS', { section: 'accounts', type: 'int', default: 90, min: 1, max: 730, desc: 'A session expires this long after the login, even if used.' });
key('MAX_SESSIONS_PER_USER', { section: 'accounts', type: 'int', default: 10, min: 1, max: 100, desc: 'Oldest sessions are revoked beyond this number.' });

// ---- Mail ------------------------------------------------------------------------------------------
key('MAIL_TRANSPORT', { section: 'mail', type: 'enum', values: ['smtp', 'log', 'none'], default: 'log',
    desc: 'smtp: send with the SMTP_* settings. log: write the messages to the log (development). none: no mail (then e-mail verification and password reset cannot work).' });
key('MAIL_FROM', { section: 'mail', type: 'string', default: 'Scacelith <no-reply@localhost>', desc: 'Sender address.' });
key('SMTP_HOST', { section: 'mail', type: 'string', default: '', desc: 'SMTP relay host.' });
key('SMTP_PORT', { section: 'mail', type: 'port', default: 587, desc: 'SMTP port (587 STARTTLS, 465 implicit TLS).' });
key('SMTP_SECURITY', { section: 'mail', type: 'enum', values: ['starttls', 'tls', 'none'], default: 'starttls',
    desc: 'starttls (required, not opportunistic), tls (implicit, port 465) or none (local relay only).' });
key('SMTP_USER', { section: 'mail', type: 'string', default: '', desc: 'SMTP user name (empty = no authentication).' });
key('SMTP_PASSWORD', { section: 'mail', type: 'secretText', default: '', desc: 'SMTP password.' });

// ---- Google single sign-on ---------------------------------------------------------------------------
key('SSO_GOOGLE_ENABLED', { section: 'sso', type: 'bool', default: false, desc: 'Offers "Sign in with Google" (OpenID Connect, authorization code + PKCE through the system browser).' });
key('GOOGLE_CLIENT_ID', { section: 'sso', type: 'string', default: '', desc: 'OAuth client ID of a "Web application" client in Google Cloud Console.' });
key('GOOGLE_CLIENT_SECRET', { section: 'sso', type: 'secretText', default: '', desc: 'OAuth client secret. Never commit it.' });
key('GOOGLE_REDIRECT_URI', { section: 'sso', type: 'string', default: '',
    desc: 'Authorized redirect URI registered at Google (default: https://SERVER_PUBLIC_HOST:PUBLIC_API_PORT/auth/sso/google/callback).' });

// ---- Abuse protection --------------------------------------------------------------------------------
key('MAX_CONNECTIONS', { section: 'limits', type: 'int', default: 200000, min: 1, desc: 'Simultaneous WebSocket connections, whole server.' });
key('MAX_CONNECTIONS_PER_IP', { section: 'limits', type: 'int', default: 16, min: 1, desc: 'Simultaneous WebSocket connections from one IP address (IPv6: per /64).' });
key('MAX_PENDING_HANDSHAKES', { section: 'limits', type: 'int', default: 128, min: 1, max: 100000,
    desc: 'TLS handshakes in progress per worker (TLS_MODE=native). A new connection beyond it, or beyond MAX_CONNECTIONS_PER_IP handshakes from one address, is closed before any TLS work and the client retries later, so a reconnection storm is served in turn instead of every handshake slowing down together. While the server is full, a worker also lets at most half this number of new TLS connections per second through.' });
key('WS_MAX_MESSAGE_BYTES', { section: 'limits', type: 'int', default: 512, min: 128, max: 65536, desc: 'Largest message a client may send.' });
key('WS_MSG_RATE', { section: 'limits', type: 'int', default: 20, min: 1, desc: 'Messages per second a client may send (sustained).' });
key('WS_MSG_BURST', { section: 'limits', type: 'int', default: 40, min: 1, desc: 'Message burst a client may send.' });
key('WS_SEND_BUFFER_LIMIT', { section: 'limits', type: 'int', default: 262144, min: 4096,
    desc: 'Bytes queued for a client that does not read; beyond it the connection is closed (the client reconnects and resynchronises).' });
key('WS_HELLO_TIMEOUT_MS', { section: 'limits', type: 'int', default: 10000, min: 1000, desc: 'Time a new connection has to authenticate.' });
key('HEARTBEAT_INTERVAL_MS', { section: 'limits', type: 'int', default: 10000, min: 1000, desc: 'Server ping interval (also measures each player\'s latency).' });
key('HEARTBEAT_TIMEOUT_MS', { section: 'limits', type: 'int', default: 30000, min: 3000, desc: 'A connection silent for this long is considered dead.' });
key('CLIENT_PING_INTERVAL_MS', { section: 'limits', type: 'int', default: 10000, min: 1000, max: 60000,
    desc: 'Interval of the game client\'s own Ping, announced in Welcome (the client measures its round trip for the ping indicator and its estimate of the server clock with it). Lower is a more reactive ping indicator but costs more server CPU for every connected player: at 2000 these pings alone take a third or more of the server CPU of a player in a 3+2 game. After each connection the client sends a few quick pings anyway.' });
key('HTTP_BODY_LIMIT', { section: 'limits', type: 'int', default: 16384, min: 1024, desc: 'Largest API request body in bytes.' });
key('HTTP_RATE_PER_IP', { section: 'limits', type: 'int', default: 120, min: 1, desc: 'API requests per minute from one IP address (all endpoints).' });
key('AUTH_RATE_PER_IP', { section: 'limits', type: 'int', default: 20, min: 1, desc: 'Login / register / reset attempts per 10 minutes from one IP address (one IPv6 /64).' });
key('AUTH_RATE_PER_PREFIX', { section: 'limits', type: 'int', default: 0, min: 0,
    desc: 'The AUTH_RATE_PER_IP limits (login / register / reset attempts, and account changes that ask for the password), per 10 minutes for one IPv6 /48 as a whole, on top of the limit of each of its /64 networks: a /48 holds 65536 of them, and one customer often gets a /56 or a /48. 0 means 5 x AUTH_RATE_PER_IP. Raise it for a site that brings many players at once over one IPv6 prefix (a campus, a club event). IPv4 addresses are only limited one by one.' });
key('AUTH_FAILURES_PER_ACCOUNT', { section: 'limits', type: 'int', default: 5, min: 1,
    desc: 'Failed logins on one account before each further attempt is delayed exponentially (up to 15 minutes).' });
key('POW_REGISTER_BITS', { section: 'limits', type: 'int', default: 18, min: 0, max: 26, desc: 'Proof-of-work difficulty (leading zero bits of SHA-256) required to register; 0 disables it.' });
key('POW_LOGIN_BITS', { section: 'limits', type: 'int', default: 18, min: 0, max: 26, desc: 'Proof-of-work difficulty required to log in while the server sees a credential-stuffing wave; 0 disables it.' });
key('POW_LOGIN_TRIGGER_PER_MIN', { section: 'limits', type: 'int', default: 30, min: 1,
    desc: 'Failed logins per minute (whole server) that turn on the login proof-of-work. Every failed login costs a password hash (about 0.5 s of CPU), so 30 per minute already keeps a quarter of a core busy, and a few hundred would need several cores: with PASSWORD_HASH_CONCURRENCY at 1 per worker, a small server could never reach such a trigger. Raise it only on a large server where honest typos alone come near it.' });
key('PASSWORD_HASH_CONCURRENCY', { section: 'limits', type: 'int', default: 1, min: 1, max: 64,
    desc: 'Password hashes and verifications (login, registration, password change and reset, account changes that ask for the password) that one worker process runs at once. Each costs about 0.5 s of CPU and 64-128 MiB in the libuv thread pool; 1 leaves the rest of the core to the games of the worker. Keep it below UV_THREADPOOL_SIZE (4 by default) so that the journal and DNS keep free threads: the server warns at start (and check-config) when it is not.' });
key('PASSWORD_HASH_QUEUE_MAX', { section: 'limits', type: 'int', default: 32, min: 0,
    desc: 'Password hashes that may wait for a free slot in one worker process; one more is refused at once with 503 server_busy and a Retry-After of 5 to 15 s (0: no waiting at all). One client (an IPv4 address, or an IPv6 /48) may have at most 2 of them waiting; its next one is refused with 429 rate_limited.' });
key('PASSWORD_HASH_QUEUE_TIMEOUT_MS', { section: 'limits', type: 'int', default: 10000, min: 100, max: 13000,
    desc: 'Longest wait for a password hash slot, for all the hashes of one request together (a password change hashes twice); the request is then refused with 503 server_busy. At most 13000: the game gives up after 15 s, and the hash itself takes a second or two, so that the player sees the "busy" answer rather than a timeout.' });

// ---- Games -------------------------------------------------------------------------------------------
key('RATED_CATEGORIES', { section: 'games', type: 'list', default: '1+0,3+0,3+2,5+0,5+3,10+0,10+5,15+10,30+0,30+20,90+30',
    desc: 'Official time controls (minutes+increment seconds). Each one has its own Elo rating; any other time control is "Custom" and never rated.' });
key('ALLOW_CUSTOM_TIME_CONTROLS', { section: 'games', type: 'bool', default: true, desc: 'Direct challenges and private games may use custom (unrated) time controls.' });
key('FIRST_MOVE_TIMEOUT_MS', { section: 'games', type: 'int', default: 30000, min: 5000, desc: 'A player who does not make their first move in time: the game is aborted (no rating change).' });
key('RECONNECT_GRACE_MIN_MS', { section: 'games', type: 'int', default: 15000, min: 5000, desc: 'Shortest time a disconnected player has to come back.' });
key('RECONNECT_GRACE_MAX_MS', { section: 'games', type: 'int', default: 60000, min: 5000, desc: 'Longest time a disconnected player has to come back (the grace is 10% of the base time within these bounds).' });
key('RECOVERY_GRACE_MS', { section: 'games', type: 'int', default: 90000, min: 15000, max: 3600000,
    desc: 'Time both players of a game restored from the journal after a restart or a crash have to come back (or the normal grace when it is longer): the server, not the players, broke the connection, and every client reconnects at once. The running clock still restarts at the recovery.' });
key('LAG_COMP_MAX_MS', { section: 'games', type: 'int', default: 1000, min: 0, max: 5000, desc: 'Largest network lag given back on one move.' });
key('LAG_QUOTA_INITIAL_MS', { section: 'games', type: 'int', default: 2000, min: 0, desc: 'Lag compensation budget of each player at the start of a game.' });
key('LAG_QUOTA_GAIN_MS', { section: 'games', type: 'int', default: 100, min: 0, desc: 'Lag compensation budget regained at every move.' });
key('LAG_QUOTA_MAX_MS', { section: 'games', type: 'int', default: 3000, min: 0, desc: 'Largest lag compensation budget.' });
key('DRAW_OFFERS_PER_GAME', { section: 'games', type: 'int', default: 3, min: 0, desc: 'Draw offers one player may make in a game.' });
key('CHALLENGE_TTL_MS', { section: 'games', type: 'int', default: 60000, min: 5000, desc: 'A direct challenge expires after this long.' });
key('PRIVATE_GAME_TTL_MS', { section: 'games', type: 'int', default: 900000, min: 60000, desc: 'A private game code expires after this long.' });

// ---- Matchmaking and ratings ------------------------------------------------------------------------
key('INITIAL_RATING', { section: 'matchmaking', type: 'int', default: 1500, min: 100, max: 3000, desc: 'Rating of a new player in every category.' });
key('PROVISIONAL_GAMES', { section: 'matchmaking', type: 'int', default: 30, min: 0, max: 100,
    desc: 'Games in a category during which the rating is provisional (K = 40, shown with "?").' });
key('MATCH_TICK_MS', { section: 'matchmaking', type: 'int', default: 250, min: 50, max: 5000, desc: 'Interval between pairing rounds.' });
key('MATCH_WINDOW_START', { section: 'matchmaking', type: 'int', default: 100, min: 0, desc: 'Largest rating difference accepted right after joining the queue.' });
key('MATCH_WINDOW_STEP', { section: 'matchmaking', type: 'int', default: 50, min: 0, desc: 'Widening of the window at each step.' });
key('MATCH_WINDOW_STEP_MS', { section: 'matchmaking', type: 'int', default: 5000, min: 100, desc: 'Time between two widenings.' });
key('MATCH_WINDOW_MAX', { section: 'matchmaking', type: 'int', default: 500, min: 0, desc: 'Widest window (reached after about a minute with the defaults).' });
key('MATCH_PROVISIONAL_BONUS', { section: 'matchmaking', type: 'int', default: 150, min: 0, desc: 'Extra window for a provisional rating (its value is still uncertain).' });
key('MATCH_REPEAT_LIMIT', { section: 'matchmaking', type: 'int', default: 3, min: 1,
    desc: 'Rated games two players may be paired for within MATCH_REPEAT_WINDOW_MS by the matchmaker (limits rating manipulation between friends).' });
key('MATCH_REPEAT_WINDOW_MS', { section: 'matchmaking', type: 'int', default: 3600000, min: 60000, desc: 'See MATCH_REPEAT_LIMIT.' });
key('CONDUCT_ABANDON_LIMIT', { section: 'matchmaking', type: 'int', default: 3, min: 1,
    desc: 'Abandoned / aborted / no-show games in 24 hours before rated matchmaking is paused for the player (15 min, then 1 h, then 6 h).' });

// ---- Anti-cheat and sanctions -------------------------------------------------------------------------
key('AUTO_SANCTION_CERTAIN_CHEATS', { section: 'anticheat', type: 'bool', default: true,
    desc: 'A technically certain cheat (forged protocol, illegal move in a synchronised position, playing out of turn) loses the game, disconnects the player and bans them for BAN_DURATION_HOURS.' });
key('BAN_DURATION_HOURS', { section: 'anticheat', type: 'int', default: 24, min: 1, max: 87600, desc: 'Length of an automatic ban.' });
key('ANALYSIS_ENGINE_PATH', { section: 'anticheat', type: 'path', default: '',
    desc: 'UCI engine (Stockfish) used to analyse rated games after they end. Empty: engine-based statistics are disabled (timing and reports still count).' });
key('ANALYSIS_WORKERS', { section: 'anticheat', type: 'int', default: 1, min: 0, max: 64, desc: 'Engine processes analysing games (each uses one core, at low priority).' });
key('ANALYSIS_DEPTH_FAST', { section: 'anticheat', type: 'int', default: 10, min: 4, max: 30, desc: 'Shallow analysis depth (a weak engine\'s choice).' });
key('ANALYSIS_DEPTH_DEEP', { section: 'anticheat', type: 'int', default: 18, min: 6, max: 40, desc: 'Deep analysis depth (a strong engine\'s choice).' });
key('ANALYSIS_MIN_PLIES', { section: 'anticheat', type: 'int', default: 30, min: 10, desc: 'Shorter games are not analysed.' });
key('REPORTS_PER_DAY', { section: 'anticheat', type: 'int', default: 5, min: 1, desc: 'Reports one player may file per day.' });
key('ANALYSIS_HASH_MB', { section: 'anticheat', type: 'int', default: 32, min: 1, max: 4096, desc: 'Transposition table of each analysis engine, in MB.' });
key('ANALYSIS_POSITION_TIMEOUT_MS', { section: 'anticheat', type: 'int', default: 120000, min: 1000,
    desc: 'Longest search of one position; an engine that exceeds it is restarted and the game is marked failed.' });
key('ANALYSIS_POLL_MS', { section: 'anticheat', type: 'int', default: 5000, min: 100, desc: 'Interval at which an idle analysis engine looks for new games to analyse.' });
key('ANALYSIS_QUEUE_MAX', { section: 'anticheat', type: 'int', default: 5000, min: 0,
    desc: 'Most ordinary games waiting for engine analysis: while this many wait, a newly finished ordinary game is not queued (the engines could not catch up anyway). Games with a report, a suspicion signal or a moderator request are always queued and analysed first. 0 analyses only those.' });
key('ANALYSIS_SAMPLE_RATE', { section: 'anticheat', type: 'number', default: 1, min: 0, max: 1,
    desc: 'Share of the ordinary rated games queued for analysis (0 to 1, drawn at random when the game ends). Lower it when the engine cannot keep up with the games played.' });

// ---- Observability -------------------------------------------------------------------------------------
key('METRICS_PORT', { section: 'observability', type: 'port', default: 9464, desc: 'Prometheus metrics and health endpoint (plain HTTP; 0 disables it).' });
key('METRICS_BIND', { section: 'observability', type: 'string', default: '127.0.0.1', desc: 'Keep it private: 127.0.0.1 or an internal address.' });
key('METRICS_TOKEN', { section: 'observability', type: 'secret', default: '', desc: 'Optional bearer token required to read the metrics.' });
key('LOG_LEVEL', { section: 'observability', type: 'enum', values: ['debug', 'info', 'warn', 'error'], default: 'info', desc: 'Log verbosity.' });
key('LOG_FORMAT', { section: 'observability', type: 'enum', values: ['json', 'pretty'], default: 'json', desc: 'JSON lines (for log collectors) or readable text.' });
key('LOG_IP', { section: 'observability', type: 'enum', values: ['truncated', 'full', 'hashed'], default: 'truncated',
    desc: 'How client addresses appear in the logs: truncated (IPv4 /24, IPv6 /48), full, or hashed (keyed HMAC, rotated daily).' });
key('RETENTION_SECURITY_DAYS', { section: 'observability', type: 'int', default: 90, min: 1, desc: 'Security events (failed logins, anomalies without sanction) are deleted after this many days.' });
key('RETENTION_IP_DAYS', { section: 'observability', type: 'int', default: 30, min: 1, desc: 'Stored IP addresses (sessions, security events) are erased after this many days.' });
key('RETENTION_INTERVAL_MS', { section: 'observability', type: 'int', default: 3600000, min: 60000,
    desc: 'Interval of the retention purge run by the primary (expired sessions and tokens, old security events, anomalies, conduct events and failed analysis jobs, IP erasure). The first run starts about a minute after the server starts.' });

// ---------------------------------------------------------------------------------------------------------

export const CONFIG_KEYS = Object.freeze(KEYS.map((k) => Object.freeze(k)));

export class ConfigError extends Error {}

// Parses KEY=value lines. Supports comments (#), blank lines, optional "export ", and values in
// single or double quotes (double quotes understand \n, \" and \\).
export function parseEnvFile(text) {
    const out = {};
    for (const raw of text.split(/\r?\n/)) {
        const line = raw.trim();
        if (!line || line.startsWith('#')) continue;
        const m = /^(?:export\s+)?([A-Z0-9_]+)\s*=\s*(.*)$/.exec(line);
        if (!m) continue;
        let v = m[2];
        if (v.startsWith('"') && v.endsWith('"') && v.length >= 2) {
            v = v.slice(1, -1).replace(/\\(n|"|\\)/g, (_, c) => (c === 'n' ? '\n' : c));
        } else if (v.startsWith("'") && v.endsWith("'") && v.length >= 2) {
            v = v.slice(1, -1);
        } else {
            const hash = v.indexOf(' #');
            if (hash >= 0) v = v.slice(0, hash).trim();
        }
        out[m[1]] = v;
    }
    return out;
}

function decodeSecret(v) {
    const s = v.trim();
    if (/^[0-9a-fA-F]+$/.test(s) && s.length % 2 === 0) return Buffer.from(s, 'hex');
    return Buffer.from(s, 'base64');
}

function toCamel(name) {
    return name.toLowerCase().replace(/_([a-z0-9])/g, (_, c) => c.toUpperCase());
}

// Returns the frozen configuration. 'env' defaults to process.env; 'envFile' (default: the
// SCACELITH_ENV_FILE variable, else ./.env when it exists) supplies values the environment does
// not set. Values are exposed in camelCase (API_PORT -> apiPort); secrets as Buffers (keys of
// type secret) and never appear in describe()/toJSON().
export function loadConfig({ env = process.env, envFile, cwd = process.cwd() } = {}) {
    const fileName = envFile ?? env.SCACELITH_ENV_FILE ?? path.join(cwd, '.env');
    let fileVars = {};
    if (fileName && fs.existsSync(fileName)) fileVars = parseEnvFile(fs.readFileSync(fileName, 'utf8'));
    const get = (n) => (env[n] !== undefined ? env[n] : fileVars[n]);

    const cfg = {};
    const errors = [];
    for (const k of KEYS) {
        let raw = get(k.name);
        const fileRef = get(k.name + '_FILE');
        const secret = k.type === 'secret' || k.type === 'secretText';
        if ((secret || k.secretFile) && fileRef && raw === undefined) {
            try { raw = fs.readFileSync(path.resolve(cwd, fileRef), 'utf8').trim(); } catch (e) {
                errors.push(`${k.name}_FILE: cannot read ${fileRef} (${e.code || e.message})`);
                continue;
            }
        }
        const has = raw !== undefined && raw !== '';
        let v;
        if (!has) {
            if (k.required) { errors.push(`${k.name} is required (${k.desc.split('.')[0]}).`); continue; }
            v = k.default;
            if (k.type === 'list') v = String(v ?? '');
            if (secret) v = null;
            if (secret || v === undefined) { cfg[toCamel(k.name)] = v ?? null; continue; }
            raw = String(v);
        }
        switch (k.type) {
            case 'string': case 'path':
                v = String(raw);
                if (k.max && v.length > k.max) errors.push(`${k.name}: at most ${k.max} characters.`);
                if (k.type === 'path' && v) v = path.resolve(cwd, v);
                break;
            case 'int': case 'port': {
                if (!/^-?\d+$/.test(String(raw).trim())) { errors.push(`${k.name}: integer expected, got "${raw}".`); continue; }
                v = parseInt(raw, 10);
                const min = k.type === 'port' ? 0 : k.min, max = k.type === 'port' ? 65535 : k.max;
                if (min !== undefined && v < min) errors.push(`${k.name}: at least ${min}.`);
                if (max !== undefined && v > max) errors.push(`${k.name}: at most ${max}.`);
                break;
            }
            case 'number': {
                if (!/^-?(\d+\.?\d*|\.\d+)$/.test(String(raw).trim())) { errors.push(`${k.name}: number expected, got "${raw}".`); continue; }
                v = Number(raw);
                if (k.min !== undefined && v < k.min) errors.push(`${k.name}: at least ${k.min}.`);
                if (k.max !== undefined && v > k.max) errors.push(`${k.name}: at most ${k.max}.`);
                break;
            }
            case 'bool': {
                const s = String(raw).trim().toLowerCase();
                if (['1', 'true', 'yes', 'on'].includes(s)) v = true;
                else if (['0', 'false', 'no', 'off'].includes(s)) v = false;
                else { errors.push(`${k.name}: true or false expected.`); continue; }
                break;
            }
            case 'enum':
                v = String(raw).trim();
                if (!k.values.includes(v)) errors.push(`${k.name}: one of ${k.values.join(', ')}.`);
                break;
            case 'list':
                v = String(raw).split(',').map((s) => s.trim()).filter(Boolean);
                break;
            case 'secretText':      // a password or client secret used as written (not decoded)
                v = String(raw);
                break;
            case 'secret':
                v = decodeSecret(String(raw));
                if (k.minBytes && v.length < k.minBytes) errors.push(`${k.name}: at least ${k.minBytes} bytes of entropy (hex or base64).`);
                break;
            default:
                throw new Error(`config: unknown type ${k.type}`);
        }
        cfg[toCamel(k.name)] = v;
    }

    // Derived values and cross-checks.
    cfg.dataDir = cfg.dataDir || path.resolve(cwd, 'data');
    cfg.dbPath = cfg.dbPath || path.join(cfg.dataDir, 'scacelith.db');
    cfg.journalDir = cfg.journalDir || path.join(cfg.dataDir, 'journal');
    cfg.runDir = path.join(cfg.dataDir, 'run');
    if (cfg.wsPort === null || cfg.wsPort === undefined) cfg.wsPort = cfg.apiPort;
    cfg.publicApiPort = cfg.publicApiPort || cfg.apiPort;
    cfg.publicWsPort = cfg.publicWsPort || cfg.wsPort;
    cfg.instanceId = cfg.instanceId || os.hostname();
    if (!cfg.authRatePerPrefix && Number.isInteger(cfg.authRatePerIp)) cfg.authRatePerPrefix = 5 * cfg.authRatePerIp;
    const w = String(cfg.workers).trim().toLowerCase();
    if (w === 'auto') cfg.workers = Math.max(1, Math.min(16, os.availableParallelism ? os.availableParallelism() : os.cpus().length));
    else if (/^\d+$/.test(w) && +w >= 1 && +w <= 64) cfg.workers = +w;
    else errors.push('WORKERS: "auto" or a number from 1 to 64.');
    if (cfg.shardBase + cfg.workers > 64) errors.push('SHARD_BASE + WORKERS must not exceed 64 (game ids hold 6 bits of shard).');
    if (cfg.tlsMode === 'native' && (!cfg.tlsCertFile || !cfg.tlsKeyFile)) errors.push('TLS_MODE=native needs TLS_CERT_FILE and TLS_KEY_FILE.');
    if (cfg.tlsMode === 'off' && !cfg.allowInsecureDev) errors.push('TLS_MODE=off is refused unless ALLOW_INSECURE_DEV=1 (never on a public server).');
    if (cfg.usernameMin > cfg.usernameMax) errors.push('USERNAME_MIN must not exceed USERNAME_MAX.');
    if (cfg.ssoGoogleEnabled && (!cfg.googleClientId || !cfg.googleClientSecret)) errors.push('SSO_GOOGLE_ENABLED needs GOOGLE_CLIENT_ID and GOOGLE_CLIENT_SECRET.');
    if (cfg.mailTransport === 'smtp' && !cfg.smtpHost) errors.push('MAIL_TRANSPORT=smtp needs SMTP_HOST.');
    if (cfg.analysisDepthFast >= cfg.analysisDepthDeep) errors.push('ANALYSIS_DEPTH_FAST must be lower than ANALYSIS_DEPTH_DEEP.');
    if (!cfg.googleRedirectUri) {
        const port = cfg.publicApiPort === 443 ? '' : `:${cfg.publicApiPort}`;
        cfg.googleRedirectUri = `https://${cfg.serverPublicHost}${port}/auth/sso/google/callback`;
    }
    const cats = [];
    for (const c of cfg.ratedCategories) {
        const m = /^(\d{1,3})\+(\d{1,3})$/.exec(c);
        if (!m || +m[1] < 1 || +m[1] > 180 || +m[2] > 180) { errors.push(`RATED_CATEGORIES: "${c}" is not minutes+seconds (e.g. 3+2).`); continue; }
        cats.push(Object.freeze({ id: c, baseMs: +m[1] * 60000, incMs: +m[2] * 1000 }));
    }
    cfg.categories = Object.freeze(cats);

    if (errors.length) throw new ConfigError('Invalid configuration:\n  - ' + errors.join('\n  - '));
    Object.defineProperty(cfg, 'toJSON', { value: () => describe(cfg), enumerable: false });
    return Object.freeze(cfg);
}

/**
 * Settings that are valid but work against the design, as sentences for the operator (logged once
 * at start by the primary, and printed by check-config). `env` is the real process environment:
 * UV_THREADPOOL_SIZE only counts there, not in the .env file.
 * @param {object} cfg a loaded configuration
 * @param {object} [env]
 * @returns {string[]}
 */
export function configWarnings(cfg, env = process.env) {
    const out = [];
    const pool = Number(env.UV_THREADPOOL_SIZE) || 4;
    if (cfg.passwordHashConcurrency >= pool) {
        out.push(`PASSWORD_HASH_CONCURRENCY (${cfg.passwordHashConcurrency}) is not below the size of the libuv thread pool `
            + `(${pool} threads, from UV_THREADPOOL_SIZE or the default 4): password hashes can then take every thread, `
            + 'and the journal\'s writes and the DNS lookups wait behind them. Lower it, or raise UV_THREADPOOL_SIZE in the process environment.');
    }
    return out;
}

// The configuration without secrets (for logs and the admin CLI).
export function describe(cfg) {
    const out = {};
    for (const [k, v] of Object.entries(cfg)) {
        const spec = KEYS.find((s) => toCamel(s.name) === k);
        if (spec && (spec.type === 'secret' || spec.type === 'secretText')) out[k] = v ? '<set>' : '<unset>';
        else out[k] = v;
    }
    return out;
}

// A configuration for tests: in-memory friendly defaults, a random secret, TLS off.
export function testConfig(overrides = {}) {
    const env = {
        SERVER_SECRET: Buffer.alloc(48, 7).toString('base64'),
        TLS_MODE: 'off', ALLOW_INSECURE_DEV: '1', WORKERS: '1', MAIL_TRANSPORT: 'none',
        POW_REGISTER_BITS: '0', POW_LOGIN_BITS: '0', LOG_LEVEL: 'error', METRICS_PORT: '0',
        ...overrides,
    };
    return loadConfig({ env, envFile: '' });
}
