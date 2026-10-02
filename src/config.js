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
import { ipMatcher } from './net/ip.js';

const KEYS = [];
function key(name, spec) { KEYS.push({ name, ...spec }); }

// ---- Server identity and network ----------------------------------------------------------------
key('SERVER_NAME', { section: 'server', type: 'string', default: 'Scacelith Community Server', max: 64, maxBytes: 64,
    desc: 'Name shown to players (menus, scoresheet "Event").' });
key('SERVER_PUBLIC_HOST', { section: 'server', type: 'string', default: 'localhost',
    desc: 'Public DNS name of the server, used in e-mail links and the Google SSO redirect URI.' });
key('SERVER_MOTD', { section: 'server', type: 'string', default: '', max: 200,
    desc: 'Short message of the day shown in the online menu.' });
key('BIND_ADDRESS', { section: 'server', type: 'string', default: '0.0.0.0', desc: 'Address the API and WebSocket listeners bind to.' });
key('API_PORT', { section: 'server', type: 'port', default: 443,
    desc: 'HTTPS API port (TCP). 443, the HTTPS port: firewalls and proxies let it through; any free port works for a community server. A port below 1024 needs the CAP_NET_BIND_SERVICE capability (README, systemd unit) unless the server runs as root.' });
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
    desc: 'Event-loop delay (p99, ms) above which a worker counts as overloaded: new games are then hosted by the least loaded worker. The delay is sampled every 10 ms and includes that period (an idle worker reads about 10 ms), so a value below about 20 marks every worker overloaded.' });

// ---- TLS -----------------------------------------------------------------------------------------
key('TLS_MODE', { section: 'tls', type: 'enum', values: ['native', 'proxy', 'off'], default: 'native',
    desc: 'native: this server terminates TLS with TLS_CERT_FILE/TLS_KEY_FILE. proxy: a reverse proxy (nginx, haproxy, caddy) terminates TLS and forwards plain HTTP/WebSocket to this server on a private address. off: plain text, refused unless ALLOW_INSECURE_DEV=1 (local development only).' });
key('TLS_CERT_FILE', { section: 'tls', type: 'path', default: '', desc: 'PEM certificate chain (fullchain). Reloaded on SIGHUP and when the file changes.' });
key('TLS_KEY_FILE', { section: 'tls', type: 'path', default: '', desc: 'PEM private key. Never commit it.' });
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
key('JOURNAL_FSYNC', { section: 'storage', type: 'bool', default: true, desc: 'fsync the journal at every flush (survives power loss, not only process crashes). With false, a flush that holds a compaction snapshot is still fsynced (with the directory when its segment is new), because the segments that snapshot replaces are deleted: a power loss then loses only the last records written, never whole games.' });
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
    desc: 'Authorized redirect URI registered at Google (default: https://SERVER_PUBLIC_HOST/auth/sso/google/callback, with :PUBLIC_API_PORT after the host when that port is not 443).' });

// ---- Protection per address (background layer, net/ipguard.js) -------------------------------------
// Every request and every connection, before routing and before TLS; quotas per signed-in account
// are the 'limits' section's business. Share = max(1, min(L, ceil(2 L / WORKERS))) per worker.
key('HTTP_RATE_PER_IP', { section: 'abuse', type: 'int', default: 600, min: 1,
    desc: 'HTTP requests per minute from one IPv4 address or IPv6 /64, whole server: every request (API, pages, health checks, unknown paths, WebSocket upgrades), counted before routing and before authentication. Each worker allows its share (all of it with 1 or 2 workers, 2 x HTTP_RATE_PER_IP / WORKERS beyond), with a burst of half a minute. A background ceiling, not a quota: it is loose enough for a school or a mobile operator that puts many players behind one address, and signed-in players are limited per account as well. Beyond it: 429 rate_limited with Retry-After.' });
key('HTTP_RATE_PER_PREFIX', { section: 'abuse', type: 'int', default: 0, min: 0,
    desc: 'The same for one IPv6 /48 as a whole, on top of the limit of each of its /64 networks: a /48 holds 65536 of them, and one customer often gets a /56 or a /48. 0 means 4 x HTTP_RATE_PER_IP; a value you set must be at least HTTP_RATE_PER_IP. IPv4 addresses are only counted one by one.' });
key('IP_CONN_RATE', { section: 'abuse', type: 'int', default: 10, min: 1, max: 100000,
    desc: 'New connections per second from one IPv4 address or IPv6 /64 (4 times that per /48), whole server, each worker its share, with a burst of 4 seconds (TLS_MODE=native: checked before any TLS work; a connection beyond it is closed with a reset). The game opens one or two connections per player.' });
key('IP_MAX_CONNECTIONS', { section: 'abuse', type: 'int', default: 128, min: 1, max: 1000000,
    desc: 'Open connections (TLS handshakes, API keep-alive connections and WebSockets together) from one IPv4 address or IPv6 /64 (4 times that per /48), each worker its share (TLS_MODE=native; a new connection beyond it is closed with a reset before TLS). Keep it at least twice MAX_CONNECTIONS_PER_IP (check-config warns otherwise).' });
key('IP_MAX_INFLIGHT', { section: 'abuse', type: 'int', default: 32, min: 1, max: 100000,
    desc: 'HTTP requests being processed at once in one worker for one IPv4 address or IPv6 /64 (4 times that per /48); one more gets 429 rate_limited. It bounds what one address can keep waiting (slow request bodies, the password hash queue).' });
key('ABUSE_BLOCK_REFUSALS_PER_MIN', { section: 'abuse', type: 'int', default: 600, min: 0,
    desc: 'Refusals in one minute, all workers together, that block an IPv4 address or IPv6 /64 before TLS (4 times that for a /48): rate-limit 429s, connections refused before TLS, malformed requests and failed TLS handshakes. A blocked address gets its new connections closed with a reset before any TLS work, and 429 with Connection: close on the connections it already has; WebSocket connections already open are kept, so that the players of a school or a mobile operator keep their games when one of them floods. The block starts at ABUSE_BLOCK_BASE_SEC and is 4 times longer at each repeat within 6 hours, up to ABUSE_BLOCK_MAX_SEC. 0 = never block (the per-address limits still apply).' });
key('ABUSE_BLOCK_BASE_SEC', { section: 'abuse', type: 'int', default: 60, min: 1, max: 86400,
    desc: 'First block of an address, in seconds (see ABUSE_BLOCK_REFUSALS_PER_MIN).' });
key('ABUSE_BLOCK_MAX_SEC', { section: 'abuse', type: 'int', default: 3600, min: 1, max: 604800,
    desc: 'Longest block of an address, in seconds (at least ABUSE_BLOCK_BASE_SEC).' });
key('ABUSE_EXEMPT', { section: 'abuse', type: 'list', default: '',
    desc: 'Addresses and CIDR subnets (203.0.113.7, 2001:db8::/48) never blocked and outside HTTP_RATE_PER_IP, IP_CONN_RATE, IP_MAX_CONNECTIONS and IP_MAX_INFLIGHT: a school or club network, monitoring, a load generator. Login, registration, the other route limits and the per-account quotas still apply. An invalid entry stops the start.' });

// ---- Abuse protection --------------------------------------------------------------------------------
key('MAX_CONNECTIONS', { section: 'limits', type: 'int', default: 200000, min: 1,
    desc: 'Simultaneous players, whole server. A newcomer beyond it still completes the TLS handshake and the WebSocket upgrade, then is refused at Hello (ServerFull, counted in scacelith_ws_hello_total{result="server_full"}, the metric that shows a full server), and the game waits 60 to 120 s before it tries again. A player whose game is in progress is still admitted, so that a full server does not make them lose it by abandonment. For such a player to reach Hello, WebSocket upgrades may go max(16, 2 %) beyond it; beyond that reserve an upgrade gets HTTP 503, and with TLS_MODE=native the TLS gate starts shedding (see MAX_PENDING_HANDSHAKES).' });
key('MAX_CONNECTIONS_PER_IP', { section: 'limits', type: 'int', default: 64, min: 1,
    desc: 'Simultaneous WebSocket connections from one IP address (IPv6: per /64), whole server. 64 lets a class or a mobile operator\'s shared address (carrier-grade NAT) play; one live connection per account still applies, and IP_MAX_CONNECTIONS bounds every connection of an address before TLS.' });
key('MAX_PENDING_HANDSHAKES', { section: 'limits', type: 'int', default: 128, min: 2, max: 100000,
    desc: 'TLS handshakes in progress per worker (TLS_MODE=native). A new connection takes a slot once the first record of its ClientHello has arrived; it has 3 s for that and holds no slot meanwhile. A connection beyond this cap, or beyond MAX_PENDING_HANDSHAKES_PER_IP for its address group, is closed before any TLS work and the client retries later, so a reconnection storm is served in turn instead of every handshake slowing down together. A worker also sheds load, letting at most half this number of new TLS connections per second through, for up to 5 s after the primary refused a WebSocket upgrade because MAX_CONNECTIONS and its reserve are in use, or while the worker holds 1.2 times its share of MAX_CONNECTIONS. It does not shed at MAX_CONNECTIONS itself, so that a player coming back to a game in progress does not compete with newcomers for that rate: each newcomer then completes the handshake and gets ServerFull at Hello.' });
key('MAX_PENDING_HANDSHAKES_PER_IP', { section: 'limits', type: 'int', min: 1, max: 99999,
    desc: 'TLS handshakes in progress per worker for one address group: an IPv4 address or an IPv6 /48 (TLS_MODE=native). Empty (the default) = MAX_PENDING_HANDSHAKES / 32 with a floor of 2, but always below MAX_PENDING_HANDSHAKES (4 by default; check-config prints the value in use). A value you set must be lower than MAX_PENDING_HANDSHAKES, so that a few hosts cannot hold every handshake slot. A worker also keeps at most 4 times this number of connections of one group waiting for their ClientHello (and 16 times MAX_PENDING_HANDSHAKES in total). Raise it when many players share one public address (a school or company network); a handshake takes a fraction of a second, so a small value still serves many players.' });
key('WS_MAX_MESSAGE_BYTES', { section: 'limits', type: 'int', default: 512, min: 128, max: 65536, desc: 'Largest message a client may send.' });
key('WS_MSG_RATE', { section: 'limits', type: 'int', default: 20, min: 1, desc: 'Messages per second a client may send (sustained).' });
key('WS_MSG_BURST', { section: 'limits', type: 'int', default: 40, min: 1, desc: 'Message burst a client may send.' });
key('WS_SEND_BUFFER_LIMIT', { section: 'limits', type: 'int', default: 262144, min: 4096,
    desc: 'Bytes queued for a client that does not read; beyond it the connection is closed (the client reconnects and resynchronises).' });
key('WS_HELLO_TIMEOUT_MS', { section: 'limits', type: 'int', default: 10000, min: 1000, max: 600000, desc: 'Time a new connection has to authenticate.' });
key('HEARTBEAT_INTERVAL_MS', { section: 'limits', type: 'int', default: 10000, min: 1000,
    desc: 'Server ping interval: each connection gets a ping every half interval to one interval (it also measures each player\'s latency). The game client sends a Ping of its own after 1.5 times this with nothing received (at least 7.5 s, at most 90 s), and considers the connection dead after twice this (at least 10 s, at most 120 s).' });
key('HEARTBEAT_TIMEOUT_MS', { section: 'limits', type: 'int', default: 30000, min: 3000, desc: 'A connection silent for this long is considered dead.' });
key('CLIENT_PING_INTERVAL_MS', { section: 'limits', type: 'int', default: 10000, min: 1000, max: 60000,
    desc: 'Interval of the game client\'s own Ping, announced in Welcome (the client measures its round trip for the ping indicator and its estimate of the server clock with it). Lower is a more reactive ping indicator but costs more server CPU for every connected player: at 2000 these pings alone take a third or more of the server CPU of a player in a 3+2 game. After each connection the client sends a few quick pings anyway.' });
key('GESTURE_RATE', { section: 'limits', type: 'int', default: 4, min: 0, max: 60,
    desc: 'Live gestures (the player\'s head, the piece in hand and where it is aimed) a client may send per second, sustained, announced in Welcome. The server relays each one to the opponent as it is and never stores it; it costs server CPU for every player in a game (docs/SIZING.md). A client beyond it has its gestures dropped silently, and only a gross excess closes the connection as a flood. 0 turns the relay off (the clients then send none).' });
key('GESTURE_BURST', { section: 'limits', type: 'int', default: 8, min: 1, max: 120,
    desc: 'Gestures a client may send in a burst above GESTURE_RATE (the size of its own token bucket, apart from WS_MSG_RATE: gestures never delay or rate-limit moves).' });
key('HTTP_BODY_LIMIT', { section: 'limits', type: 'int', default: 16384, min: 1024,
    desc: 'Largest API request body in bytes, except POST /api/v1/gif, which has its own fixed limit of 135,168 bytes (a PGN of up to 64 KiB as a JSON string, escapes included).' });
key('AUTH_RATE_PER_IP', { section: 'limits', type: 'int', default: 20, min: 1, desc: 'Login / register / reset attempts per 10 minutes from one IP address (one IPv6 /64).' });
key('AUTH_RATE_PER_PREFIX', { section: 'limits', type: 'int', default: 0, min: 0,
    desc: 'The AUTH_RATE_PER_IP limits (login / register / reset attempts, and account changes that ask for the password), per 10 minutes for one IPv6 /48 as a whole, on top of the limit of each of its /64 networks: a /48 holds 65536 of them, and one customer often gets a /56 or a /48. 0 means 5 x AUTH_RATE_PER_IP. Raise it for a site that brings many players at once over one IPv6 prefix (a campus, a club event). IPv4 addresses are only limited one by one.' });
key('AUTH_FAILURES_PER_ACCOUNT', { section: 'limits', type: 'int', default: 5, min: 1,
    desc: 'Failed logins on one account before each further attempt is delayed exponentially (up to 15 minutes).' });
key('AUTH_REGISTER_PER_HOUR', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'Registrations per hour from one IPv4 address or IPv6 /64 (3 times that per IPv6 /48), whole server, on top of AUTH_RATE_PER_IP. Raise it for a session where a class creates its accounts together.' });
key('AUTH_MAIL_PER_HOUR', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'Confirmation e-mails asked again (POST /auth/verify-email/resend) per hour from one IPv4 address or IPv6 /64 (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP and of the one e-mail per address every 5 minutes. Password reset e-mails have their own, stricter limits (AUTH_FORGOT_PER_HOUR, AUTH_FORGOT_PER_DAY).' });
key('AUTH_FORGOT_PER_HOUR', { section: 'limits', type: 'int', default: 3, min: 1,
    desc: 'Password reset e-mails asked (POST /auth/password/forgot) per hour from one IPv4 address or IPv6 /64 (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP and of the one e-mail per address every 5 minutes. A refusal is a 429, which says nothing about the address; an accepted request answers 202 whether the address has an account or not.' });
key('AUTH_FORGOT_PER_DAY', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'The same as AUTH_FORGOT_PER_HOUR per 24 hours (3 times that per /48). At least AUTH_FORGOT_PER_HOUR.' });
key('AUTH_RESET_PER_HOUR', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'New passwords sent with a reset link (POST /auth/password/reset and the /reset-password page) per hour from one IPv4 address or IPv6 /64 (3 times that per /48), whole server, on top of AUTH_RATE_PER_IP: each one hashes a password.' });
key('AUTH_MFA_PER_ACCOUNT', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'Second-factor codes (authenticator or recovery codes) tried per 15 minutes for one account, whole server, from any address, at sign-in and in account changes; then 429 too_many_attempts before the code is checked (a recovery code is not spent). It bounds a code guesser who knows the password, whatever the number of addresses.' });
key('AUTH_REAUTH_PER_USER', { section: 'limits', type: 'int', default: 10, min: 1,
    desc: 'Account changes that ask for the password or a code (password, two-step verification, e-mail, data export, deletion) per 10 minutes for one account, whole server, from any address, on top of the per-address limit AUTH_RATE_PER_IP: a stolen session used from many addresses cannot guess the password faster.' });
key('USER_RATE_PER_MIN', { section: 'limits', type: 'int', default: 120, min: 1,
    desc: 'API requests per minute of one signed-in account (every request with a valid session token), all endpoints together, whatever its address. Each worker allows its share, max(1, min(this, ceil(2 x this / WORKERS))) (all of it with 1 or 2 workers, half with 4), with a burst of half a minute; beyond it 429 rate_limited with Retry-After. The game\'s busiest use, paging through the history, is about one request per second.' });
key('POW_REGISTER_BITS', { section: 'limits', type: 'int', default: 18, min: 0, max: 26, desc: 'Proof-of-work difficulty (leading zero bits of SHA-256) required to register; 0 disables it.' });
key('POW_LOGIN_BITS', { section: 'limits', type: 'int', default: 18, min: 0, max: 26, desc: 'Proof-of-work difficulty required to log in while the server sees a credential-stuffing wave; 0 disables it.' });
key('POW_LOGIN_TRIGGER_PER_MIN', { section: 'limits', type: 'int', default: 30, min: 1,
    desc: 'Failed logins per minute (whole server) that turn on the login proof-of-work. Every failed login costs a password hash (about 0.5 s of CPU), so 30 per minute already keeps a quarter of a core busy, and a few hundred would need several cores: with PASSWORD_HASH_CONCURRENCY at 1 per worker, a small server could never reach such a trigger. Raise it only on a large server where honest typos alone come near it.' });
key('PASSWORD_HASH_CONCURRENCY', { section: 'limits', type: 'int', default: 1, min: 1, max: 64,
    desc: 'Password hashes and verifications (login, registration, password change and reset, account changes that ask for the password) that one worker process runs at once. Each costs about 0.5 s of CPU and 64-128 MiB in the libuv thread pool; 1 leaves the rest of the core to the games of the worker. Keep it below UV_THREADPOOL_SIZE (4 by default) so that the journal and DNS keep free threads: the server warns at start (and check-config) when it is not.' });
key('PASSWORD_HASH_QUEUE_MAX', { section: 'limits', type: 'int', default: 32, min: 0,
    desc: 'Password hashes that may wait for a free slot in one worker process; one more is refused at once with 503 server_busy and a Retry-After of 5 to 15 s (0: no waiting at all). Once half of them wait, one client (an IPv4 address, or an IPv6 /48) may have at most PASSWORD_HASH_WAITERS_PER_SOURCE of them waiting; its next one is refused with 429 rate_limited.' });
key('PASSWORD_HASH_WAITERS_PER_SOURCE', { section: 'limits', type: 'int', default: 2, min: 1,
    desc: 'Password hashes one client (an IPv4 address, or an IPv6 /48) may have waiting in one worker process once PASSWORD_HASH_QUEUE_MAX is at least half full; its next request is then refused with 429 rate_limited and a Retry-After of 5 to 15 s, and that refused attempt does not count against AUTH_RATE_PER_IP. While less than half of the queue waits, one client may queue more, so that players who log in together behind one address (a school or a company network) are served when the server is not busy, and one client never holds more than half of the queue. Raise it for such a site if its players log in while the server is busy, together with MAX_PENDING_HANDSHAKES_PER_IP and AUTH_RATE_PER_IP.' });
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
    desc: 'Time both players of a game restored from the journal after a restart or a crash have to come back (or the normal grace when it is longer): the server, not the players, broke the connection, and every client reconnects at once. The clock of the side to move stays stopped until that player is back, for RECOVERY_CLOCK_HOLD_MS at most.' });
key('RECOVERY_CLOCK_HOLD_MS', { section: 'games', type: 'int', default: 20000, min: 0, max: 3599999,
    desc: 'After a restart or a crash, the clock (or the first-move timer) of the side to move of a restored game does not run until that player is back, and runs again after this long even if they are still away. When it is not set, it is 20000, or RECOVERY_GRACE_MS - 1 when RECOVERY_GRACE_MS is 20000 or less; a value you set must be lower than RECOVERY_GRACE_MS. It bounds the free thinking time a player could get by staying away on purpose; 0 restarts the clock at the recovery.' });
key('LAG_COMP_MAX_MS', { section: 'games', type: 'int', default: 1000, min: 0, max: 5000, desc: 'Largest network lag given back on one move.' });
key('LAG_QUOTA_INITIAL_MS', { section: 'games', type: 'int', default: 2000, min: 0, desc: 'Lag compensation budget of each player at the start of a game.' });
key('LAG_QUOTA_GAIN_MS', { section: 'games', type: 'int', default: 100, min: 0, desc: 'Lag compensation budget regained at every move.' });
key('LAG_QUOTA_MAX_MS', { section: 'games', type: 'int', default: 3000, min: 0, desc: 'Largest lag compensation budget.' });
key('GAME_STALL_MIN_MS', { section: 'games', type: 'int', default: 30, min: 5, max: 1000,
    desc: 'A worker whose event loop stopped for longer than this (garbage collection, blocking I/O, CPU steal) counts as stalled: the game requests that waited in its sockets meanwhile (moves, resignations, draw offers and answers, claims, aborts, Resyncs and the closing of a connection) are handled before its timers, as if they had arrived when the stall began, so that a flag or a first-move timeout that fell during the stall does not overtake them; a first-move timeout that fell during it records no no-show against the player. Shorter pauses change nothing.' });
key('GAME_STALL_CREDIT_MAX_MS', { section: 'games', type: 'int', default: 5000, min: 0, max: 60000,
    desc: 'Longest stall of a worker that is not charged to the players (see GAME_STALL_MIN_MS): a message handled after a longer stall counts as arrived this long before it was read. It is also the most a player can gain from one stall. 0 charges every stall to the side to move, as a server without this protection would.' });
key('AUTO_PRESS_CLOCK', { section: 'games', type: 'bool', default: true,
    desc: 'The players\' robots press the clock by themselves once a move is on the board. When false, a client sends its move only when its player presses the clock, so the mover\'s clock runs until then. Decided when a game is created (a rematch keeps the value of the game it follows) and kept by the game, restarts included.' });
key('DRAW_OFFERS_PER_GAME', { section: 'games', type: 'int', default: 3, min: 0, desc: 'Draw offers one player may make in a game.' });
key('CHALLENGE_TTL_MS', { section: 'games', type: 'int', default: 60000, min: 5000, desc: 'A direct challenge expires after this long.' });
key('PRIVATE_GAME_TTL_MS', { section: 'games', type: 'int', default: 900000, min: 60000, desc: 'A private game code expires after this long.' });

// ---- Matchmaking and ratings ------------------------------------------------------------------------
key('INITIAL_RATING', { section: 'matchmaking', type: 'int', default: 1500, min: 100, max: 3000,
    desc: 'Working rating of a new player in every category: shown and used for pairing until their first rating, which FIDE\'s rules compute after five counted games (a game lost before the loser\'s first draw or win counts for neither player: docs/DESIGN.md, ratings), and what an unrated opponent counts for in those games.' });
key('PROVISIONAL_GAMES', { section: 'matchmaking', type: 'int', default: 30, min: 0, max: 100,
    desc: 'Counted games in a category (those that entered the rating: the five of the unrated phase, then the games against rated opponents) during which the rating is provisional: K = 40, shown with "?" and off the leaderboard (an unrated player is always provisional). FIDE uses 30.' });
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
key('RATING_REFUND_DAYS', { section: 'anticheat', type: 'int', default: 60, min: 0, max: 3650,
    desc: 'When a player is banned as a cheater (a certain cheat, or a moderator\'s integrity confirm), each opponent who lost rating points to them in a rated game that ended within this many days before the ban gets those points back on their current rating (docs/ANTICHEAT.md, rating refunds). A game still in progress at the ban (or not recorded yet) is refunded when it is recorded, while that ban lasts (not after an integrity confirm --no-refund; a user ban refunds nothing). 0: no refunds unless a moderator asks for them (scacelith-admin refunds apply).' });
key('ANALYSIS_ENGINE_PATH', { section: 'anticheat', type: 'path', default: '',
    desc: 'UCI engine used to analyse rated games after they end: the official Stockfish 19 release binary for Linux x86-64, the engine the anti-cheat is calibrated for (docs/ANTICHEAT.md, section 3). Empty: engine-based statistics are disabled (timing and reports still count). Another engine or network restarts the statistics (they are kept per analysis profile).' });
key('ANALYSIS_WORKERS', { section: 'anticheat', type: 'int', default: 1, min: 0, max: 64, desc: 'Engine processes analysing games (each uses one core, at low priority). Stockfish 19 engines share one copy of their network: about 70 MB per engine after the first, 180 MB when /tmp is not writable (docs/SIZING.md).' });
key('ANALYSIS_DEPTH_FAST', { section: 'anticheat', type: 'int', default: 9, min: 4, max: 30,
    desc: 'Shallow analysis depth (a weak engine\'s choice). Changing it restarts the statistics, like ANALYSIS_DEPTH_DEEP.' });
key('ANALYSIS_DEPTH_DEEP', { section: 'anticheat', type: 'int', default: 15, min: 6, max: 40,
    desc: 'Deep analysis depth (a strong engine\'s choice). On a VPS vCore (AVX2), Stockfish 19 at 9/15 costs 14 % less than Stockfish 16 at the former 10/18, and flags engine users earlier than at 9/14, 9/16 or 10/18 in most cases (docs/ANTICHEAT.md, calibration). Changing it restarts the statistics (they are kept per analysis profile: engine, network, depths, hash).' });
key('ANALYSIS_MIN_PLIES', { section: 'anticheat', type: 'int', default: 30, min: 10, desc: 'Shorter games are not analysed.' });
key('REPORTS_PER_DAY', { section: 'anticheat', type: 'int', default: 5, min: 1, desc: 'Reports one player may file per day.' });
key('ANALYSIS_HASH_MB', { section: 'anticheat', type: 'int', default: 32, min: 1, max: 4096,
    desc: 'Transposition table of each analysis engine, in MB. Changing it restarts the statistics, like ANALYSIS_DEPTH_DEEP.' });
key('ANALYSIS_POSITION_TIMEOUT_MS', { section: 'anticheat', type: 'int', default: 120000, min: 1000, max: 3600000,
    desc: 'Longest search of one position; an engine that exceeds it is restarted and the game is marked failed.' });
key('ANALYSIS_POLL_MS', { section: 'anticheat', type: 'int', default: 5000, min: 100, max: 3600000, desc: 'Interval at which an idle analysis engine looks for new games to analyse.' });
key('ANALYSIS_QUEUE_MAX', { section: 'anticheat', type: 'int', default: 5000, min: 0, max: 100000,
    desc: 'Most ordinary games waiting for engine analysis: while this many wait, a newly finished ordinary game is not queued (the engines could not catch up anyway). Games with a report, a suspicion signal (at most 20 waiting per player) or a moderator request are queued anyway and mostly analysed first; one engine claim in four still goes to the oldest ordinary game. 0 analyses only those (and then no game feeds the population statistics). At most 100000: the waiting ordinary games are counted in every commit of finished games, under the database write lock.' });
key('ANALYSIS_SAMPLE_RATE', { section: 'anticheat', type: 'number', default: 1, min: 0, max: 1,
    desc: 'Share of the ordinary rated games queued for analysis (0 to 1, drawn at random when the game ends). Lower it when the engine cannot keep up with the games played.' });

// ---- Animated GIFs of games ------------------------------------------------------------------------------
key('GIF_ENABLED', { section: 'gif', type: 'bool', default: true,
    desc: 'Animated GIFs of games for signed-in players: GET /api/v1/games/:id/gif (a game of this server) and POST /api/v1/gif (any game, as a PGN). Rendered on a thread of each worker process, never on the event loop of the games. false: both endpoints answer 404 gif_disabled.' });
key('GIF_THREADS', { section: 'gif', type: 'int', default: 1, min: 1, max: 8,
    desc: 'GIF renders at the same time in one worker process, each on a thread of its own at the lowest CPU priority (on Linux: it only takes the CPU the games leave). A render takes one core (measured on a 2.1 GHz Xeon: about 45 ms for a 40-move game at the medium size, about 0.7 s for a game of GIF_MAX_PLIES at the large size; docs/SIZING.md), and a thread 40-50 MiB of memory while it lives (up to about 125 MiB after many of the longest games at the large size). The threads start on demand and stop after a minute without work.' });
key('GIF_QUEUE_MAX', { section: 'gif', type: 'int', default: 4, min: 0, max: 64,
    desc: 'Renders that may wait for a free thread in one worker process; one more is refused at once with 503 server_busy and Retry-After, and the render quotas it took are given back.' });
key('GIF_QUEUE_TIMEOUT_MS', { section: 'gif', type: 'int', default: 10000, min: 100, max: 60000,
    desc: 'Longest wait of a render for a free thread; then 503 server_busy (the render quotas are given back).' });
key('GIF_RENDER_TIMEOUT_MS', { section: 'gif', type: 'int', default: 30000, min: 1000, max: 120000,
    desc: 'Longest render: the thread is stopped (a new one starts with the next render) and the request answers 500.' });
key('GIF_MAX_PLIES', { section: 'gif', type: 'int', default: 600, min: 1, max: 1200,
    desc: 'Longest game, in half-moves, a GIF shows; a longer one answers 422 game_too_long. 600 plies last 5 minutes at the default speed (0.5 s per move).' });
key('GIF_CACHE_MB', { section: 'gif', type: 'int', default: 32, min: 0, max: 1024,
    desc: 'Memory of the cache of rendered GIFs in each worker process (the least recently used goes first; a medium GIF of 80 plies is about 200 KiB). A GIF served from the cache costs no render quota. 0 disables the cache.' });
key('GIF_USER_RENDERS_PER_MIN', { section: 'gif', type: 'int', default: 4, min: 1,
    desc: 'GIF renders per minute of one account, whole server (a GIF served from the cache does not count); beyond it 429 rate_limited with Retry-After.' });
key('GIF_USER_RENDERS_PER_HOUR', { section: 'gif', type: 'int', default: 30, min: 1,
    desc: 'GIF renders per hour of one account, whole server. At least GIF_USER_RENDERS_PER_MIN.' });
key('GIF_IP_RENDERS_PER_MIN', { section: 'gif', type: 'int', default: 12, min: 1,
    desc: 'GIF renders per minute from one IPv4 address or IPv6 /64 (3 times that per IPv6 /48), all accounts together, whole server: a background ceiling for many accounts behind one address.' });
key('GIF_IP_RENDERS_PER_HOUR', { section: 'gif', type: 'int', default: 120, min: 1,
    desc: 'GIF renders per hour from one IPv4 address or IPv6 /64 (3 times that per /48), all accounts together, whole server. At least GIF_IP_RENDERS_PER_MIN.' });

// ---- Observability -------------------------------------------------------------------------------------
key('METRICS_PORT', { section: 'observability', type: 'port', default: 9464, desc: 'Prometheus metrics and health endpoint (plain HTTP; 0 disables it).' });
key('METRICS_BIND', { section: 'observability', type: 'string', default: '127.0.0.1', desc: 'Keep it private: 127.0.0.1 or an internal address.' });
key('METRICS_TOKEN', { section: 'observability', type: 'secretText', default: '',
    desc: 'Optional bearer token required to read the metrics: /metrics then needs the header "Authorization: Bearer <token>" with this exact text (no spaces).' });
key('LOG_LEVEL', { section: 'observability', type: 'enum', values: ['debug', 'info', 'warn', 'error'], default: 'info', desc: 'Log verbosity.' });
key('LOG_FORMAT', { section: 'observability', type: 'enum', values: ['json', 'pretty'], default: 'json', desc: 'JSON lines (for log collectors) or readable text.' });
key('LOG_IP', { section: 'observability', type: 'enum', values: ['truncated', 'full', 'hashed'], default: 'truncated',
    desc: 'How client addresses appear in the logs: truncated (IPv4 /24, IPv6 /48), full, or hashed (keyed HMAC, rotated daily).' });
key('RETENTION_SECURITY_DAYS', { section: 'observability', type: 'int', default: 90, min: 1, desc: 'Security events (failed logins, anomalies without sanction) are deleted after this many days.' });
key('RETENTION_IP_DAYS', { section: 'observability', type: 'int', default: 30, min: 1, desc: 'Stored IP addresses (sessions, security events) are erased after this many days.' });
key('RETENTION_INTERVAL_MS', { section: 'observability', type: 'int', default: 3600000, min: 60000, max: 2147483647,
    desc: 'Interval of the retention purge run by the primary (expired sessions and tokens, old security events, anomalies, conduct events and failed analysis jobs, IP erasure). The first run starts about a minute after the server starts. At most 2147483647 (about 24.8 days, the longest timer of Node.js).' });

// ---------------------------------------------------------------------------------------------------------

export const CONFIG_KEYS = Object.freeze(KEYS.map((k) => Object.freeze(k)));

/**
 * Default MAX_PENDING_HANDSHAKES_PER_IP: MAX_PENDING_HANDSHAKES / 32 with a floor of 2, but below
 * the total when the total is at least 2 (4 for the default 128, 1 for a total of 2). loadConfig
 * stores the result, so check-config prints the value the TLS gate uses (net/listeners.js).
 * @param {number} maxPending MAX_PENDING_HANDSHAKES
 */
export function defaultPendingPerGroup(maxPending) {
    return Math.max(1, Math.min(maxPending - 1, Math.max(2, Math.floor(maxPending / 32))));
}

export class ConfigError extends Error {}

// Parses KEY=value lines. Supports comments (#), blank lines, optional "export ", and values in
// single or double quotes (double quotes understand \n, \" and \\). A quoted value followed by a
// comment keeps its quotes; `notes`, when given, receives a sentence for each such line.
export function parseEnvFile(text, notes = null) {
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
            if (hash >= 0) {
                v = v.slice(0, hash).trim();
                if (notes && (v[0] === '"' || v[0] === "'")) {
                    notes.push(`${m[1]}: the value is quoted and followed by a comment on the same line of the .env file, `
                        + 'so its quotes are part of the value. Put the comment on a line of its own (check the value first: '
                        + 'a secret changes when its quotes go).');
                }
            }
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
// type secret) and never appear in describe()/toJSON(). `rawValues` (not enumerable) holds the
// text of every key that was set, *_FILE contents included: loadConfig({ env: rawValues,
// envFile: '' }) gives the same configuration without reading any file again (the shards'
// configuration, cluster/worker-main.js).
export function loadConfig({ env = process.env, envFile, cwd = process.cwd() } = {}) {
    const named = envFile ?? env.SCACELITH_ENV_FILE;            // '' = no file
    const fileName = named ?? path.join(cwd, '.env');
    const cfg = {};
    const errors = [];
    const notes = [];           // configWarnings sentences found while loading
    const rawValues = {};
    let fileVars = {};
    if (named) {
        // A file named explicitly must be there: a typo would start the server on the defaults.
        try { fileVars = parseEnvFile(fs.readFileSync(named, 'utf8'), notes); } catch (e) {
            errors.push(`SCACELITH_ENV_FILE: cannot read ${named} (${e.code || e.message}).`);
        }
    } else if (fileName && fs.existsSync(fileName)) fileVars = parseEnvFile(fs.readFileSync(fileName, 'utf8'), notes);
    const get = (n) => (env[n] !== undefined ? env[n] : fileVars[n]);

    // The key is read from TLS_KEY_FILE already; its text must never become a path (and a log line).
    const keyFileRef = get('TLS_KEY_FILE_FILE');
    if (keyFileRef !== undefined && keyFileRef !== '') errors.push('TLS_KEY_FILE already names the key file; TLS_KEY_FILE_FILE is not supported.');
    for (const k of KEYS) {
        let raw = get(k.name);
        const fileRef = get(k.name + '_FILE');
        const secret = k.type === 'secret' || k.type === 'secretText';
        // An empty KEY= (the .env.example line of a required secret, an unset docker-compose
        // variable) does not hide KEY_FILE for a required secret; an optional one stays unset.
        if (secret && fileRef && raw === '' && !k.required) {
            notes.push(`${k.name} is set but empty, so ${k.name}_FILE is not read and ${k.name} is unset. `
                + `Remove the empty ${k.name}= (or ${k.name}_FILE) to say which one you mean.`);
        }
        if (secret && fileRef && (raw === undefined || (raw === '' && k.required))) {
            try { raw = fs.readFileSync(path.resolve(cwd, fileRef), 'utf8').trim(); } catch (e) {
                errors.push(`${k.name}_FILE: cannot read ${fileRef} (${e.code || e.message})`);
                continue;
            }
        }
        if (raw !== undefined) rawValues[k.name] = raw;
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
                else if (k.maxBytes && Buffer.byteLength(v, 'utf8') > k.maxBytes) errors.push(`${k.name}: at most ${k.maxBytes} bytes in UTF-8 (fewer characters with accents or other scripts).`);
                if (k.maxBytes && v.includes('\0')) errors.push(`${k.name}: no NUL character.`);
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
    // Protection per address (net/ipguard.js): the /48 budget defaults to 4 /64s' worth.
    if (Number.isInteger(cfg.httpRatePerIp)) {
        if (!cfg.httpRatePerPrefix) cfg.httpRatePerPrefix = 4 * cfg.httpRatePerIp;
        else if (cfg.httpRatePerPrefix < cfg.httpRatePerIp) errors.push('HTTP_RATE_PER_PREFIX must be 0 or at least HTTP_RATE_PER_IP (a /48 holds many /64 networks).');
    }
    if (cfg.abuseBlockBaseSec > cfg.abuseBlockMaxSec) errors.push('ABUSE_BLOCK_BASE_SEC must not exceed ABUSE_BLOCK_MAX_SEC.');
    try { ipMatcher(cfg.abuseExempt); } catch (e) { errors.push(`ABUSE_EXEMPT: ${e.message}.`); }
    if (cfg.tlsMode === 'proxy') { try { ipMatcher(cfg.trustedProxies); } catch (e) { errors.push(`TRUSTED_PROXIES: ${e.message}.`); } }
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
    if (cfg.authForgotPerDay < cfg.authForgotPerHour) errors.push('AUTH_FORGOT_PER_DAY must be at least AUTH_FORGOT_PER_HOUR.');
    if (cfg.gifUserRendersPerHour < cfg.gifUserRendersPerMin) errors.push('GIF_USER_RENDERS_PER_HOUR must be at least GIF_USER_RENDERS_PER_MIN.');
    if (cfg.gifIpRendersPerHour < cfg.gifIpRendersPerMin) errors.push('GIF_IP_RENDERS_PER_HOUR must be at least GIF_IP_RENDERS_PER_MIN.');
    if (cfg.maxPendingHandshakesPerIp != null && cfg.maxPendingHandshakesPerIp >= cfg.maxPendingHandshakes) {
        errors.push('MAX_PENDING_HANDSHAKES_PER_IP must be lower than MAX_PENDING_HANDSHAKES (one address group could otherwise hold every handshake slot).');
    }
    // Empty: the effective default, so that check-config shows it and the TLS gate uses the same value.
    if (cfg.maxPendingHandshakesPerIp == null && Number.isInteger(cfg.maxPendingHandshakes)) {
        cfg.maxPendingHandshakesPerIp = defaultPendingPerGroup(cfg.maxPendingHandshakes);
    }
    // The hold must end before the grace. Only a value the operator set is refused; the default
    // follows a short RECOVERY_GRACE_MS down.
    const holdRaw = get('RECOVERY_CLOCK_HOLD_MS');
    if (holdRaw !== undefined && holdRaw !== '') {
        if (cfg.recoveryClockHoldMs >= cfg.recoveryGraceMs) errors.push('RECOVERY_CLOCK_HOLD_MS must be lower than RECOVERY_GRACE_MS.');
    } else if (Number.isInteger(cfg.recoveryGraceMs)) {
        cfg.recoveryClockHoldMs = Math.min(cfg.recoveryClockHoldMs, cfg.recoveryGraceMs - 1);
    }
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
    Object.defineProperty(cfg, 'loadNotes', { value: Object.freeze(notes), enumerable: false });
    Object.defineProperty(cfg, 'rawValues', { value: Object.freeze(rawValues), enumerable: false });
    return Object.freeze(cfg);
}

/**
 * The size of the libuv thread pool for a value of UV_THREADPOOL_SIZE, read as libuv reads it:
 * unset is 4; otherwise atoi() (leading digits, anything else is 0), 0 becomes 1 thread, and a
 * negative value (unsigned in libuv) or one above 1024 becomes 1024.
 * @param {string|undefined} raw
 * @returns {number}
 */
export function threadPoolSize(raw) {
    if (raw === undefined || raw === null) return 4;
    const n = parseInt(String(raw), 10);
    if (!Number.isFinite(n) || n === 0) return 1;
    if (n < 0 || n > 1024) return 1024;
    return n;
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
    const out = [...(cfg.loadNotes || [])];
    const pool = threadPoolSize(env.UV_THREADPOOL_SIZE);
    if (cfg.passwordHashConcurrency >= pool) {
        out.push(`PASSWORD_HASH_CONCURRENCY (${cfg.passwordHashConcurrency}) is not below the size of the libuv thread pool `
            + `(${pool === 1 ? '1 thread' : `${pool} threads`}, from UV_THREADPOOL_SIZE or the default 4; libuv reads an empty, 0 or `
            + 'unreadable value as 1 thread): password hashes can then take every thread, '
            + 'and the journal\'s writes and the DNS lookups wait behind them. Lower it, or raise UV_THREADPOOL_SIZE in the process environment.');
    }
    if (cfg.ipMaxConnections < 2 * cfg.maxConnectionsPerIp) {
        out.push(`IP_MAX_CONNECTIONS (${cfg.ipMaxConnections}) is below twice MAX_CONNECTIONS_PER_IP (${cfg.maxConnectionsPerIp}): `
            + 'the players behind one address (a school, a mobile operator) could be refused before TLS while their WebSockets '
            + 'and API connections are still within MAX_CONNECTIONS_PER_IP. Raise IP_MAX_CONNECTIONS, or list the address in ABUSE_EXEMPT.');
    }
    // A connection that only answers the pings is silent for up to an interval plus the sweeper
    // tick (250 ms) when the router checks it (cluster/router.js heartbeat), plus round trip and
    // event-loop lag.
    if (cfg.heartbeatTimeoutMs < cfg.heartbeatIntervalMs + 2250) {
        out.push(`HEARTBEAT_TIMEOUT_MS (${cfg.heartbeatTimeoutMs}) is less than HEARTBEAT_INTERVAL_MS (${cfg.heartbeatIntervalMs}) + 2250: `
            + 'healthy idle connections, which only answer the server\'s pings, would be closed as silent (\'timeout\') and reconnect. '
            + 'Raise HEARTBEAT_TIMEOUT_MS (the default is 3 times the interval).');
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
