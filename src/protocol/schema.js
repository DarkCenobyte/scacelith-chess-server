// Scacelith realtime protocol: the single source of truth.
//
// tools/gen-protocol.js turns this file into
//   src/protocol/codec.gen.js          (server and Node client/bench codec)
//   ../src/net/protocol_gen.h / .cpp   (game client codec, C++17)
//   docs/PROTOCOL.md                   (message reference)
// and computes SCHEMA_HASH over it. Never edit the generated files by hand; change this file and
// run `npm run gen:protocol`. Any change to the wire format must bump PROTOCOL_VERSION (and the
// server keeps accepting older versions it still supports: PROTOCOL_MIN).
//
// ---- Wire format ------------------------------------------------------------------------------
// One protocol message per WebSocket *binary* message (text frames are a protocol violation).
//   u8  type             message id (table below; 0x01-0x7F client->server, 0x80-0xFF server->client)
//   ... fields           in the order listed, little-endian, no padding
// Every client->server message starts with `seq` (u32): 1 for Hello, then +1 for each message on
// the connection. The server drops (and counts) a message whose seq is not exactly last+1.
// Errors and acks quote it as `ref`.
//
// Field types:
//   u8 u16 u32 i32   unsigned / signed integers
//   f64              IEEE-754 double (wall-clock milliseconds since the Unix epoch)
//   id53             game id: u64 on the wire, value < 2^53 (JS number, C++ uint64_t); 0 = none
//   bool             u8, 0 or 1 (anything else is malformed)
//   str8             u8 byte length + UTF-8 (valid UTF-8 required, no NUL); opts.max bytes (<= 255), opts.min
//   enum:Name        u8, must be one of the enum's values
//   struct:Name      the struct's fields inline
//   list16:T         u16 count + items (T = a scalar type or struct:Name); opts.max items
// Numeric opts.min / opts.max are validated on decode (out of range = malformed).
// A message must be consumed exactly: trailing bytes are malformed.
//
// Moves (u16): from | to << 6 | promo << 12, squares 0..63 (a1 = 0, h8 = 63), promo = 0 none,
// 2 knight, 3 bishop, 4 rook, 5 queen (chess::PieceType numbering). Castling is the king's move
// (e1g1). Bit 15 must be 0.
//
// Position digest (posHash, u32): FNV-1a 32 of the ASCII text made of the first four FEN fields
// "placement side castling ep" separated by single spaces, where ep is written only when an en
// passant capture is actually legal ("-" otherwise) and castling is "KQkq" order or "-". This is
// exactly the prefix of chess::Position::fen() in the game.

export const PROTOCOL_VERSION = 2;
export const PROTOCOL_MIN = 2;
export const WS_SUBPROTOCOL = 'scacelith.v1';

export const enums = {
    Color: { White: 0, Black: 1, None: 2 },
    ColorPref: { Random: 0, White: 1, Black: 2 },
    GameStatus: { Ongoing: 0, WhiteWins: 1, BlackWins: 2, Draw: 3, Aborted: 4 },
    // Values 0..13 are chess::GameEndReason in the game (src/chess/chess.h); 20+ are online only.
    EndReason: {
        None: 0, Checkmate: 1, Resignation: 2, Timeout: 3, IllegalMoves: 4, Stalemate: 5,
        InsufficientMaterial: 6, TimeoutVsInsufficient: 7, FivefoldRepetition: 8, SeventyFiveMoves: 9,
        ThreefoldClaim: 10, FiftyMoveClaim: 11, Agreement: 12, IllegalMovesVsInsufficient: 13,
        Abandonment: 20,              // disconnected longer than the grace period: loss
        AbandonmentVsInsufficient: 21,// ...but the opponent cannot mate: draw
        Aborted: 22,                  // aborted by a player before their first move (unrated)
        NoShow: 23,                   // first move not made in time (aborted, unrated)
        Forfeit: 24,                  // certain cheat: loss
        ServerAborted: 25,            // server could not continue the game (unrated)
        BothDisconnected: 26,         // both players vanished at the same time (aborted, unrated)
    },
    GameEventKind: {
        DrawOffered: 1,       // color = offerer
        DrawDeclined: 2,      // color = decliner (also sent when the opponent moves instead of answering)
        PlayerDisconnected: 3,// color = who; arg = grace in ms
        PlayerReconnected: 4, // color = who
        RematchOffered: 5,    // color = offerer (after the end)
        RematchDeclined: 6,   // color = decliner, or None when it expired
        AbortAvailable: 7,    // reserved
    },
    QueueState: { Left: 0, Searching: 1, Matched: 2 },
    ChallengeState: { Pending: 0, Accepted: 1, Declined: 2, Cancelled: 3, Expired: 4, Unavailable: 5 },
    NoticeCode: {
        ServerShutdown: 1,        // arg = ms before the shutdown
        Banned: 2,                // arg = end of the ban (epoch ms)
        SessionRevoked: 3,
        MatchmakingCooldown: 4,   // arg = end of the cooldown (epoch ms)
        ReplacedByNewConnection: 5,
        Motd: 6,                  // reserved (the message of the day comes from /api/v1/info)
        RatingRestored: 7,        // arg = rating points given back: an opponent of your rated games was banned for cheating
    },
    ErrorCode: {
        Malformed: 1, UnsupportedProtocol: 2, Unauthorized: 3, Banned: 4, RateLimited: 5,
        ServerFull: 6, Replaced: 7, ShuttingDown: 8, Internal: 9, HelloRequired: 10,
        EmailUnverified: 11,
        NotInGame: 100, NotYourTurn: 101, IllegalMove: 102, StalePly: 103, Desync: 104,
        GameOver: 105, AlreadyInGame: 106, InvalidCategory: 107, DrawOfferLimit: 108,
        NothingToClaim: 109, AbortNotAllowed: 110, NoPendingOffer: 111, FlagFell: 112,
        QueueNotAllowed: 200, ChallengeNotFound: 201, UserUnavailable: 202, ChallengeLimit: 203,
        CannotChallengeSelf: 204, CodeInvalid: 205, RatedRequiresOfficialTc: 206,
        MatchmakingCooldown: 207, InvalidTimeControl: 208, RematchUnavailable: 209,
        // Enums travel as u8: keep every value below 256.
        ProtocolViolation: 240, Flood: 241, CheatDetected: 242, SlowConsumer: 243,
    },
};

// Move flags (MoveMade.flags, bit set).
export const MoveFlag = { Capture: 1, EnPassant: 2, CastleKing: 4, CastleQueen: 8, DoublePush: 16, Promotion: 32, Check: 64, Mate: 128 };

// Gesture flags (Gesture.flags, bit set). Side: the look falls on the table beside the board (the
// clock, the captured pieces or the scoresheet): each client puts the clock at its own player's
// right, so the other client mirrors that look.
export const GestureFlag = { Glance: 1, Promoting: 2, Side: 4 };

// WebSocket close codes used by the server (4000 + ErrorCode where one applies).
export const CloseCode = {
    Normal: 1000, GoingAway: 1001, ProtocolError: 1002, Unsupported: 1003, Policy: 1008, TooBig: 1009, Internal: 1011,
    UnsupportedProtocol: 4002, Unauthorized: 4003, Banned: 4004, ServerFull: 4006, Replaced: 4007, ShuttingDown: 4008,
    HelloTimeout: 4010, ProtocolViolation: 4300, Flood: 4301, CheatDetected: 4302, SlowConsumer: 4303,
};

export const structs = {
    PlayerInfo: [
        ['userId', 'u32'],
        ['name', 'str8', { max: 24, min: 1 }],
        ['rating', 'u16'],
        ['provisional', 'bool'],
    ],
    MoveRec: [
        ['move', 'u16', { max: 0x7fff }],
        ['spentMs', 'u32'],   // clock time charged for this move (after lag compensation)
        ['clockMs', 'u32'],   // mover's remaining time after the move, increment included
    ],
    RatingChange: [
        ['before', 'u16'],
        ['after', 'u16'],
        ['games', 'u32'],     // rated games in the category, this one included
        ['provisional', 'bool'],
    ],
};

const C2S = 'c2s', S2C = 's2c';

export const messages = [
    // ---- connection ----
    { id: 0x01, name: 'Hello', dir: C2S, doc: 'First message of a connection. The token is the session token from the HTTPS login of *this* server.',
      fields: [['seq', 'u32'], ['proto', 'u16'], ['schema', 'u32'], ['client', 'str8', { max: 48 }], ['token', 'str8', { min: 16, max: 160 }]] },
    { id: 0x02, name: 'Ping', dir: C2S, doc: 'Client round-trip measurement (at most one per second); answered by Pong with the server clock.',
      fields: [['seq', 'u32'], ['nonce', 'u32']] },
    { id: 0x03, name: 'Pong', dir: C2S, doc: 'Answer to the server Ping (echo its nonce at once).',
      fields: [['seq', 'u32'], ['nonce', 'u32']] },

    // ---- matchmaking, challenges ----
    { id: 0x10, name: 'QueueJoin', dir: C2S, doc: 'Join the matchmaking queue of an official category ("3+2"). rated=false: casual queue.',
      fields: [['seq', 'u32'], ['category', 'str8', { min: 3, max: 7 }], ['rated', 'bool']] },
    { id: 0x11, name: 'QueueLeave', dir: C2S, fields: [['seq', 'u32']] },
    { id: 0x12, name: 'ChallengeCreate', dir: C2S, doc: 'Challenge a player by name, or (empty target) create a private game joined with a code. Rated only with an official category.',
      fields: [['seq', 'u32'], ['target', 'str8', { max: 24 }], ['baseSec', 'u16', { min: 15, max: 10800 }], ['incSec', 'u8', { max: 180 }], ['rated', 'bool'], ['color', 'enum:ColorPref']] },
    { id: 0x13, name: 'ChallengeAccept', dir: C2S, fields: [['seq', 'u32'], ['id', 'u32']] },
    { id: 0x14, name: 'ChallengeDecline', dir: C2S, fields: [['seq', 'u32'], ['id', 'u32']] },
    { id: 0x15, name: 'ChallengeCancel', dir: C2S, fields: [['seq', 'u32'], ['id', 'u32']] },
    { id: 0x16, name: 'ChallengeJoinCode', dir: C2S, fields: [['seq', 'u32'], ['code', 'str8', { min: 4, max: 12 }]] },

    // ---- game (intents: the server decides) ----
    { id: 0x20, name: 'Move', dir: C2S,
      doc: 'Move intent for ply `ply` of game `game`. posHash is the digest of the position the client played in; thinkMs the client-measured time since the turn began (used only for bounded lag compensation). drawOffer: the move comes with a draw offer.',
      fields: [['seq', 'u32'], ['game', 'id53'], ['ply', 'u16', { max: 1199 }], ['move', 'u16', { max: 0x7fff }], ['posHash', 'u32'], ['thinkMs', 'u32'], ['drawOffer', 'bool']] },
    { id: 0x21, name: 'Resign', dir: C2S, fields: [['seq', 'u32'], ['game', 'id53']] },
    { id: 0x22, name: 'DrawOffer', dir: C2S, fields: [['seq', 'u32'], ['game', 'id53']] },
    { id: 0x23, name: 'DrawAnswer', dir: C2S, fields: [['seq', 'u32'], ['game', 'id53'], ['accept', 'bool']] },
    { id: 0x24, name: 'DrawClaim', dir: C2S, doc: 'Claim a draw by threefold repetition or the fifty-move rule in the current position.',
      fields: [['seq', 'u32'], ['game', 'id53']] },
    { id: 0x25, name: 'Abort', dir: C2S, doc: 'Abort before one\'s own first move (no rating change).', fields: [['seq', 'u32'], ['game', 'id53']] },
    { id: 0x26, name: 'Resync', dir: C2S, doc: 'Ask for a full GameSnapshot.', fields: [['seq', 'u32'], ['game', 'id53']] },
    { id: 0x27, name: 'Rematch', dir: C2S, doc: 'After the end: accept=true offers (or accepts) a rematch with colours swapped, accept=false declines or withdraws.',
      fields: [['seq', 'u32'], ['game', 'id53'], ['accept', 'bool']] },

    // ---- live gestures (cosmetic, relayed to the opponent as they are, never stored) ----
    { id: 0x28, name: 'Gesture', dir: C2S,
      doc: 'The player\'s current gestures in game `game`, sent when they change and at least once a second (at most gestureRate per second, see Welcome): the head (yaw and pitch of the look in milliradians, seat-relative: 0 = straight ahead, level, yaw > 0 to the left, pitch < 0 down; lean 0..100), the piece in hand and where it is aimed, the move placed on the board before the clock press. The whole state travels every time, so a lost one heals with the next. ply: plies played when the current state of the hand (touch, aim, placed, Promoting) began, which a change of the head alone keeps; touch / aim: squares (64 = none); placed: the move placed, packed as in Move (0 = none); flags: GestureFlag bits. The server forwards it to the opponent as a Gesture without looking at it: it never counts as a move.',
      fields: [['seq', 'u32'], ['game', 'id53'], ['ply', 'u16', { max: 1199 }], ['touch', 'u8', { max: 64 }], ['aim', 'u8', { max: 64 }],
               ['placed', 'u16', { max: 0x7fff }], ['flags', 'u8', { max: 7 }],
               ['yaw', 'i32', { min: -3142, max: 3142 }], ['pitch', 'i32', { min: -1571, max: 1571 }], ['lean', 'u8', { max: 100 }]] },

    // ---- server -> client ----
    { id: 0x80, name: 'Welcome', dir: S2C,
      doc: 'Hello accepted. When activeGame != 0 a GameSnapshot follows. heartbeatMs: interval of the server Ping; clientPingMs: interval the client should use for its own Ping (CLIENT_PING_INTERVAL_MS; 0 = the client\'s default). gestureRate / gestureBurst: the Gesture relay of this server (GESTURE_RATE, GESTURE_BURST): sustained messages per second and bucket size; 0 = no relay, send no Gesture.',
      fields: [['proto', 'u16'], ['serverTime', 'f64'], ['userId', 'u32'], ['username', 'str8', { max: 24 }], ['serverName', 'str8', { max: 64 }],
               ['heartbeatMs', 'u32'], ['clientPingMs', 'u32'], ['maxMsgPerSec', 'u16'], ['activeGame', 'id53'],
               ['gestureRate', 'u16', { max: 60 }], ['gestureBurst', 'u16', { max: 120 }]] },
    { id: 0x81, name: 'Error', dir: S2C, doc: 'A request was refused. fatal: the server closes the connection after it.',
      fields: [['ref', 'u32'], ['code', 'enum:ErrorCode'], ['fatal', 'bool'], ['game', 'id53']] },
    { id: 0x82, name: 'Ping', dir: S2C, doc: 'Heartbeat; answer with Pong at once (the server measures the latency with it).',
      fields: [['nonce', 'u32'], ['serverTime', 'f64']] },
    { id: 0x83, name: 'Pong', dir: S2C, fields: [['nonce', 'u32'], ['serverTime', 'f64']] },
    { id: 0x84, name: 'Ack', dir: S2C, doc: 'Request accepted (for requests without another answer).', fields: [['ref', 'u32']] },
    { id: 0x85, name: 'Notice', dir: S2C, fields: [['code', 'enum:NoticeCode'], ['arg', 'f64']] },

    { id: 0x90, name: 'QueueStatus', dir: S2C, doc: 'Sent on join, every few seconds while searching, and on leave/match.',
      fields: [['category', 'str8', { max: 7 }], ['rated', 'bool'], ['state', 'enum:QueueState'], ['waitMs', 'u32'], ['window', 'u16'], ['queued', 'u32']] },
    { id: 0x91, name: 'ChallengeReceived', dir: S2C, doc: 'yourColor is the colour offered to the receiver.',
      fields: [['id', 'u32'], ['from', 'struct:PlayerInfo'], ['baseSec', 'u16'], ['incSec', 'u8'], ['rated', 'bool'], ['yourColor', 'enum:ColorPref'], ['expiresMs', 'u32']] },
    { id: 0x92, name: 'ChallengeStatus', dir: S2C, doc: 'State of a challenge for its creator (and for the receiver when it is cancelled or expires). code: private game code.',
      fields: [['id', 'u32'], ['state', 'enum:ChallengeState'], ['target', 'str8', { max: 24 }], ['code', 'str8', { max: 12 }], ['baseSec', 'u16'], ['incSec', 'u8'], ['rated', 'bool']] },

    { id: 0xA0, name: 'GameSnapshot', dir: S2C,
      doc: 'Complete authoritative state of a game: sent when it starts, after a (re)connection and on Resync, and also to the opponent when the held clock of a game restored after a restart starts (lifecycle step 6). Clocks are the remaining times at serverTime; the `running` side keeps counting from there (`running` is None while such a clock is held). autoPress: the robots press the clock by themselves once a move is on the board (AUTO_PRESS_CLOCK when the game was created); when false a client sends its Move only when its player presses the clock, so the clock runs until then.',
      fields: [['game', 'id53'], ['gseq', 'u32'], ['category', 'str8', { max: 7 }], ['baseMs', 'u32'], ['incMs', 'u32'], ['rated', 'bool'],
               ['white', 'struct:PlayerInfo'], ['black', 'struct:PlayerInfo'], ['you', 'enum:Color'],
               ['moves', 'list16:struct:MoveRec', { max: 1200 }],
               ['running', 'enum:Color'], ['whiteMs', 'u32'], ['blackMs', 'u32'], ['serverTime', 'f64'],
               ['drawOffer', 'enum:Color'], ['status', 'enum:GameStatus'], ['reason', 'enum:EndReason'],
               ['whiteConnected', 'bool'], ['blackConnected', 'bool'], ['graceMs', 'u32'], ['firstMoveMs', 'u32'],
               ['startedAt', 'f64'], ['rematch', 'enum:Color'], ['autoPress', 'bool']] },
    { id: 0xA1, name: 'MoveMade', dir: S2C,
      doc: 'A move accepted by the server, sent to both players (for the mover it is the confirmation). Clocks as in GameSnapshot; firstMoveMs: time the next player has for their first move (0 when not applicable).',
      fields: [['game', 'id53'], ['gseq', 'u32'], ['ply', 'u16'], ['move', 'u16'], ['flags', 'u8'], ['spentMs', 'u32'],
               ['whiteMs', 'u32'], ['blackMs', 'u32'], ['serverTime', 'f64'], ['drawOffer', 'bool'], ['firstMoveMs', 'u32']] },
    { id: 0xA2, name: 'MoveRejected', dir: S2C, doc: 'The move intent was refused (never shown to the opponent). A GameSnapshot follows when the client must resynchronise.',
      fields: [['game', 'id53'], ['ply', 'u16'], ['move', 'u16'], ['code', 'enum:ErrorCode']] },
    { id: 0xA3, name: 'GameEvent', dir: S2C, fields: [['game', 'id53'], ['gseq', 'u32'], ['kind', 'enum:GameEventKind'], ['color', 'enum:Color'], ['arg', 'u32']] },
    { id: 0xA4, name: 'GameEnd', dir: S2C, doc: 'Final result. Ratings follow in RatingUpdate once committed (rated games).',
      fields: [['game', 'id53'], ['gseq', 'u32'], ['status', 'enum:GameStatus'], ['reason', 'enum:EndReason'], ['whiteMs', 'u32'], ['blackMs', 'u32'], ['serverTime', 'f64']] },
    { id: 0xA5, name: 'RatingUpdate', dir: S2C, doc: 'Rating changes of a finished rated game, sent after the database transaction committed.',
      fields: [['game', 'id53'], ['category', 'str8', { max: 7 }], ['white', 'struct:RatingChange'], ['black', 'struct:RatingChange']] },
    { id: 0xA6, name: 'Gesture', dir: S2C, doc: 'The opponent\'s gestures (the client Gesture minus seq, byte for byte). Cosmetic: never authoritative.',
      fields: [['game', 'id53'], ['ply', 'u16', { max: 1199 }], ['touch', 'u8', { max: 64 }], ['aim', 'u8', { max: 64 }],
               ['placed', 'u16', { max: 0x7fff }], ['flags', 'u8', { max: 7 }],
               ['yaw', 'i32', { min: -3142, max: 3142 }], ['pitch', 'i32', { min: -1571, max: 1571 }], ['lean', 'u8', { max: 100 }]] },
];
