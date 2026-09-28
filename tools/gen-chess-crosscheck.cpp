// Generates dedicated-server/test/fixtures/chess-crosscheck.json with the GAME's own rules
// (src/chess/position.cpp + game.cpp), so that the server's JavaScript rules (src/chess/*.js)
// can be checked against them move for move (test/unit/chess.crosscheck.test.js).
//
// Build and run: dedicated-server/tools/gen-chess-crosscheck.sh (from anywhere).
// Usage: gen-chess-crosscheck <assets/i18n/en.lang> <output.json>
//
// Everything is deterministic (fixed seed, own PRNG): the same sources give the same file.
//
// Output (JSON):
//   fens:  [[input, fen() after setFEN or null when refused], ...]
//   san:   [{fen, moves: [[u16, SAN], ...]}]        every legal move of hand-picked positions
//   pgnTags: tags used for the "pgn" field of games
//   games: [{start, plies: [ply...], final: pos, status, reason, hash, pgn?}]
//     start  input FEN given to chess::Game::resetFromFEN (null = standard start)
//     ply    [fen, digest, legal, move, san, flags, info]   the position BEFORE the move
//     final  [fen, digest, legal, info]                     the position after the last move
//     fen     chess::Position::fen()
//     digest  FNV-1a 32 of the first four FEN fields (the protocol posHash)
//     legal   sorted legal moves as u16 (from | to << 6 | promo << 12), little-endian, base64
//     move    u16 of the move played; san its SAN (chess::Game::sanMoves)
//     flags   chess MoveFlags of the move | 64 when it gives check | 128 when it mates
//     info    bit 0 in check, 1 dead position, 2 canColorMate(White), 3 canColorMate(Black),
//             4 canClaimThreefold, 5 canClaimFiftyMove, bits 8+ repetitionCount (chess::Game)
//     status / reason   chess::GameStatus / GameEndReason at the end (0 / 0 when ongoing)
//     hash    chess::Position::hash() of the final position (16 hex digits)
//     pgn     chess::Game::pgn("White", "Black", pgnTags) for some games
#include "chess/chess.h"

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <fstream>
#include <map>
#include <sstream>
#include <string>
#include <vector>

// ---- i18n stubs --------------------------------------------------------------------------------
// game.cpp needs i18n::tr (endReasonText) and i18n::english (PGN comment). The English texts are
// read from the game's assets/i18n/en.lang, so the PGN comments are the real ones.
namespace i18n {
static std::map<std::string, std::string> gEnglish;
const char* tr(const char* key) {
    auto it = gEnglish.find(key);
    return it == gEnglish.end() ? key : it->second.c_str();
}
const char* english(const char* key) { return tr(key); }
}  // namespace i18n

namespace {

using namespace chess;

bool loadEnglish(const char* path) {
    std::ifstream in(path, std::ios::binary);
    if (!in) return false;
    std::string line;
    auto trim = [](std::string s) {
        const size_t a = s.find_first_not_of(" \t\r\n");
        if (a == std::string::npos) return std::string();
        const size_t b = s.find_last_not_of(" \t\r\n");
        return s.substr(a, b - a + 1);
    };
    while (std::getline(in, line)) {
        const std::string t = trim(line);
        if (t.empty() || t[0] == '#') continue;
        const size_t eq = t.find('=');
        if (eq == std::string::npos) continue;
        i18n::gEnglish[trim(t.substr(0, eq))] = trim(t.substr(eq + 1));
    }
    return !i18n::gEnglish.empty();
}

struct Rng {
    uint64_t s;
    uint64_t next() {
        s += 0x9E3779B97F4A7C15ULL;
        uint64_t z = s;
        z = (z ^ (z >> 30)) * 0xBF58476D1CE4E5B9ULL;
        z = (z ^ (z >> 27)) * 0x94D049BB133111EBULL;
        return z ^ (z >> 31);
    }
    uint32_t below(uint32_t n) { return uint32_t(next() % n); }
    double unit() { return double(next() >> 11) * (1.0 / 9007199254740992.0); }
};

uint16_t u16(const Move& m) { return uint16_t(m.from | (m.to << 6) | (int(m.promotion) << 12)); }

uint32_t fnv1a32(const std::string& s) {
    uint32_t h = 0x811c9dc5u;
    for (unsigned char c : s) {
        h ^= c;
        h *= 0x01000193u;
    }
    return h;
}

uint32_t digestOf(const Position& p) {
    const std::string f = p.fen();
    // First four fields: cut before the halfmove clock (the last two space-separated fields).
    size_t cut = f.rfind(' ');
    cut = f.rfind(' ', cut - 1);
    return fnv1a32(f.substr(0, cut));
}

std::string base64(const std::vector<uint8_t>& d) {
    static const char* T = "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    std::string o;
    size_t i = 0;
    for (; i + 2 < d.size(); i += 3) {
        const uint32_t v = (uint32_t(d[i]) << 16) | (uint32_t(d[i + 1]) << 8) | d[i + 2];
        o += T[v >> 18];
        o += T[(v >> 12) & 63];
        o += T[(v >> 6) & 63];
        o += T[v & 63];
    }
    if (i + 1 == d.size()) {
        const uint32_t v = uint32_t(d[i]) << 16;
        o += T[v >> 18];
        o += T[(v >> 12) & 63];
        o += "==";
    } else if (i + 2 == d.size()) {
        const uint32_t v = (uint32_t(d[i]) << 16) | (uint32_t(d[i + 1]) << 8);
        o += T[v >> 18];
        o += T[(v >> 12) & 63];
        o += T[(v >> 6) & 63];
        o += '=';
    }
    return o;
}

std::string legalB64(const Position& p) {
    std::vector<uint16_t> ms;
    for (const Move& m : p.legalMoves()) ms.push_back(u16(m));
    std::sort(ms.begin(), ms.end());
    std::vector<uint8_t> bytes;
    for (uint16_t m : ms) {
        bytes.push_back(uint8_t(m & 0xff));
        bytes.push_back(uint8_t(m >> 8));
    }
    return base64(bytes);
}

std::string jstr(const std::string& s) {
    std::string o = "\"";
    for (char c : s) {
        switch (c) {
        case '"': o += "\\\""; break;
        case '\\': o += "\\\\"; break;
        case '\n': o += "\\n"; break;
        case '\r': o += "\\r"; break;
        case '\t': o += "\\t"; break;
        default:
            if ((unsigned char)c < 0x20) {
                char buf[8];
                std::snprintf(buf, sizeof buf, "\\u%04x", c);
                o += buf;
            } else {
                o += c;
            }
        }
    }
    return o + "\"";
}

int infoOf(const Game& g) {
    const Position& p = g.position();
    int info = 0;
    if (p.inCheck()) info |= 1;
    if (p.hasInsufficientMaterial()) info |= 2;
    if (p.canColorMate(White)) info |= 4;
    if (p.canColorMate(Black)) info |= 8;
    if (g.canClaimThreefold()) info |= 16;
    if (g.canClaimFiftyMove()) info |= 32;
    info |= g.repetitionCount() << 8;
    return info;
}

std::string posJSON(const Game& g) {
    const Position& p = g.position();
    return jstr(p.fen()) + "," + std::to_string(digestOf(p)) + "," + jstr(legalB64(p));
}

// ---- move choice ---------------------------------------------------------------------------------

enum class Policy { Biased, Repetition, Quiet, Squeeze, Uniform };

struct Scored {
    Move m;
    double w;
};

Move choose(const Game& g, Policy policy, Rng& rng, int ply, int shuffleFrom) {
    const Position& p = g.position();
    const std::vector<Move> moves = p.legalMoves();
    if (policy == Policy::Uniform) return moves[rng.below(uint32_t(moves.size()))];

    // Repetition: once shuffling, the mover undoes its own previous move when it can.
    if (policy == Policy::Repetition && ply >= shuffleFrom && g.moves().size() >= 2) {
        const Move prev = g.moves()[g.moves().size() - 2];
        for (const Move& m : moves)
            if (m.from == prev.to && m.to == prev.from && !(m.flags & (MoveCapture | MovePromotion)) && p.at(m.from).type != Pawn &&
                rng.unit() < 0.97)
                return m;
    }

    std::vector<Scored> sc;
    sc.reserve(moves.size());
    for (const Move& m : moves) {
        Position n(p);
        n.makeMove(m);
        const bool check = n.inCheck();
        const bool noMove = !n.hasLegalMove();
        double w = 1.0;
        const bool pawn = p.at(m.from).type == Pawn;
        switch (policy) {
        case Policy::Biased:
        case Policy::Repetition:
            if (m.flags & MoveCapture) w += 3;
            if (m.flags & MoveEnPassant) w += 40;
            if (m.flags & (MoveCastleKing | MoveCastleQueen)) w += 30;
            if (m.flags & MovePromotion) w += 12;
            if (m.flags & MoveDoublePush) w += 2;
            if (check) w += 3;
            if (check && noMove) w += 60;          // mate
            if (!check && noMove) w += 8;          // stalemate
            break;
        case Policy::Quiet:
            w = (pawn || (m.flags & MoveCapture)) ? 0.02 : 1.0;
            if (check && noMove) w += 1;
            break;
        case Policy::Squeeze: {
            // Fewer replies for the opponent is better: drives towards mates and stalemates.
            const size_t replies = n.legalMoves().size();
            w = 1.0 / double(1 + replies * replies);
            if (noMove) w += check ? 4.0 : 1.0;
            if (m.flags & MoveCapture) w *= 0.2;   // keep the material a little longer
            break;
        }
        default:
            break;
        }
        sc.push_back({m, w});
    }
    double total = 0;
    for (const Scored& s : sc) total += s.w;
    double r = rng.unit() * total;
    for (const Scored& s : sc) {
        r -= s.w;
        if (r <= 0) return s.m;
    }
    return sc.back().m;
}

struct GameSpec {
    const char* start;   // nullptr = standard start
    Policy policy;
    int maxPlies;
    bool pgn;
};

std::string runGame(const GameSpec& spec, Rng& rng) {
    Game g;
    if (spec.start && !g.resetFromFEN(spec.start)) {
        std::fprintf(stderr, "invalid start FEN skipped: %s\n", spec.start);
        return "";
    }
    const int shuffleFrom = int(rng.below(24));
    std::string plies;
    for (int ply = 0; ply < spec.maxPlies && !g.isOver(); ++ply) {
        const Move m = choose(g, spec.policy, rng, ply, shuffleFrom);
        const std::string head = posJSON(g);
        const int info = infoOf(g);
        if (!g.play(m)) {
            std::fprintf(stderr, "play failed\n");
            std::exit(1);
        }
        const Move played = g.moves().back();
        int flags = played.flags;
        if (g.position().inCheck()) flags |= 64;
        if (g.position().isCheckmate()) flags |= 128;
        if (!plies.empty()) plies += ",\n";
        plies += "   [" + head + "," + std::to_string(u16(played)) + "," + jstr(g.sanMoves().back()) + "," + std::to_string(flags) + "," +
                 std::to_string(info) + "]";
    }
    char hash[32];
    std::snprintf(hash, sizeof hash, "%016llx", (unsigned long long)g.position().hash());
    std::string o = "  {\"start\": " + (spec.start ? jstr(spec.start) : std::string("null")) + ",\n   \"plies\": [\n" + plies + "],\n";
    o += "   \"final\": [" + posJSON(g) + "," + std::to_string(infoOf(g)) + "],\n";
    o += "   \"status\": " + std::to_string(int(g.status())) + ", \"reason\": " + std::to_string(int(g.endReason())) + ", \"hash\": \"" + hash +
         "\"";
    if (spec.pgn) {
        PgnTags tags;
        tags.event = "Crosscheck";
        tags.site = "Scacelith";
        tags.date = "2026.01.01";
        tags.round = "-";
        tags.timeControl = "180+2";
        o += ",\n   \"pgn\": " + jstr(g.pgn("White", "Black", tags));
    }
    return o + "}";
}

// ---- hand-picked inputs --------------------------------------------------------------------------

const char* kFens[] = {
    // Valid, printed back unchanged.
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
    "rnbqkbnr/pppp1ppp/8/8/3Pp3/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 3",
    "8/8/8/8/8/8/8/K6k w - - 99 150",
    "4k3/8/8/8/8/8/8/4K2R w K - 0 1",
    "r3k3/8/8/8/8/8/8/4K3 b q - 5 40",
    // Optional counters, extra blanks, tabs and line breaks.
    "4k3/8/8/8/8/8/8/4K3 w - -",
    "4k3/8/8/8/8/8/8/4K3 w - - 7",
    "  4k3/8/8/8/8/8/8/4K3   b  -  -  3  9  ",
    "4k3/8/8/8/8/8/8/4K3\tw\t-\t-\t0\t1",
    "4k3/8/8/8/8/8/8/4K3\r\nw - - 0 1\n",
    "4k3/8/8/8/8/8/8/4K3 w - - 0 0",
    "4k3/8/8/8/8/8/8/4K3 w - - 000 0012",
    "4k3/8/8/8/8/8/8/4K3 w - - 999999999 999999999",
    "4k3/8/8/8/8/8/8/4K3 w - - 1000000000 1",
    "4k3/8/8/8/8/8/8/4K3 w - - +1 1",
    "4k3/8/8/8/8/8/8/4K3 w - - -1 1",
    "4k3/8/8/8/8/8/8/4K3 w - - 0 1 extra",
    "4k3/8/8/8/8/8/8/4K3 w -",
    "4k3/8/8/8/8/8/8/44 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K12 w - - 0 1",
    "4k3/8/8/8/8/8/8/1111K111 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K4 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K2 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K3/ w - - 0 1",
    "4k3/8/8/8/8/8/8/4K3/8 w - - 0 1",
    "4k3/8/8/8/8/8/8 w - - 0 1",
    "4k3//8/8/8/8/8/8/4K3 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K03 w - - 0 1",
    "4k3/8/8/8/8/8/8/4K9 w - - 0 1",
    "4k3/8/8/8/8/8/8/4X3 w - - 0 1",
    "",
    "   ",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP w KQkq - 0 1",
    "rnbqkbnr/pppppppp/9/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR x KQkq - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR W KQkq - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkx - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w -K - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w -- - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w qkQK - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KKQQkq - 0 1",
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w AHah - 0 1",
    "8/8/8/8/8/8/8/K7 w - - 0 1",
    "k7/8/8/8/8/8/8/KK6 w - - 0 1",
    "kk6/8/8/8/8/8/8/K7 w - - 0 1",
    "8/8/8/8/8/8/8/8 w - - 0 1",
    "P3k3/8/8/8/8/8/8/4K3 w - - 0 1",
    "4k3/8/8/8/8/8/8/p3K3 w - - 0 1",
    "4k3/pppppppp/p7/8/8/8/8/4K3 w - - 0 1",
    "4k3/8/8/8/8/8/PPPPPPPP/4K3 w - - 0 1",
    "4k3/8/8/8/8/P7/PPPPPPPP/4K3 w - - 0 1",
    "4k3/8/8/8/8/N7/PPPPPPPP/RNBQKBNR w KQ - 0 1",
    "4k3/8/8/8/8/8/PPPPPPPP/RNBQKBNR w KQ - 0 1",
    "4k3/8/8/8/8/8/8/4K2R b - - 0 1",
    "4k2R/8/8/8/8/8/8/4K3 w - - 0 1",
    "4k2R/8/8/8/8/8/8/4K3 b - - 0 1",
    "4k3/8/8/8/8/8/8/4K3 w - - x 1",
    "4k3/8/8/8/8/8/8/4K3 w - - 0 x",
    "4k3/8/8/8/8/8/8/Q3K2Q w - - 0 1",
    "3rk3/8/8/8/8/8/3Q4/3RK3 b - - 0 1",
    "4k3/8/8/1b6/8/8/8/R3K2r w - - 0 1",
    // Castling rights normalisation.
    "4k3/8/8/8/8/8/8/4K3 w KQkq - 0 1",
    "r3k3/8/8/8/8/8/8/4K2R w KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R4K1R w KQkq - 0 1",
    "1r2k1r1/8/8/8/8/8/8/R3K2R b KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R w - - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R w qQ - 0 1",
    "r3k2b/8/8/8/8/8/8/N3K2R w KQkq - 0 1",
    // En passant normalisation.
    "rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq d6 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq D6 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq e6 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d3 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d5 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq i6 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d 0 2",
    "rnbqkbnr/ppp1pppp/8/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d66 0 2",
    "rnbqkbnr/ppp1pppp/3p4/3pP3/8/8/PPPP1PPP/RNBQKBNR w KQkq d6 0 2",
    "rnbqkbnr/ppp1pppp/8/3PP3/8/8/PPP2PPP/RNBQKBNR w KQkq d6 0 2",
    "rnbqkbnr/pppp1ppp/8/8/3Pp3/8/PPP1PPPP/RNBQKBNR b KQkq d3 0 3",
    "rnbqkbnr/pppp1ppp/8/8/3Pp3/8/PPP1PPPP/RNBQKBNR w KQkq d3 0 3",
    "4k3/8/8/KPp4r/8/8/8/8 w - c6 0 2",
    "4k3/8/8/1Pp5/8/8/8/K7 w - c6 0 2",
    "4k3/8/8/1Pp4r/K7/8/8/8 w - c6 0 2",
    "8/8/8/8/k1pP3Q/8/8/4K3 b - d3 0 1",
    "8/8/8/8/2pP4/8/8/k3K2Q b - d3 0 1",
    "4k3/8/8/2pP4/8/8/8/4K1b1 w - c6 0 1",
    "4k3/8/8/2pP4/8/b7/8/4K3 w - c6 0 1",
    "8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1",
    "3k4/8/8/2Pp3r/8/8/8/2K5 w - d6 0 1",
    "4k3/8/8/2PpP3/8/8/8/4K3 w - d6 0 1",
    "4k3/4r3/8/3pP3/8/8/8/4K3 w - d6 0 1",
    "4k3/8/8/3pP3/8/8/8/4K3 w - d6 0 1",
    "4k3/8/8/8/3pP3/8/8/4K3 b - e3 0 1",
    "4k3/8/8/8/3pP3/8/8/4KR2 b - e3 0 1",
    "4k3/8/8/b7/3pP3/8/8/7K b - e3 0 1",
    "7k/8/8/8/3pP3/8/8/B6K b - e3 0 1",
    "4k3/8/8/8/3Pp3/8/8/4K3 b - d3 0 1",
};

const char* kSanFens[] = {
    "rnbqkbnr/pppppppp/8/8/8/8/PPPPPPPP/RNBQKBNR w KQkq - 0 1",
    "rnbqkb1r/ppp1pppp/5n2/3p4/8/8/PPPPPPPP/RNBQKBNR b KQkq - 0 1",
    "4k3/8/8/R7/8/8/8/R3K3 w - - 0 1",
    "1k6/8/8/8/4Q2Q/8/K7/7Q w - - 0 1",
    "1Q4Q1/8/8/8/k7/8/8/1Q2K1Q1 w - - 0 1",
    "4k3/8/8/8/8/N7/8/N3K3 w - - 0 1",
    "k7/8/8/2N1N3/1N3N2/8/1N3N2/K1N5 w - - 0 1",
    "3k4/8/8/8/8/8/1K6/R6R w - - 0 1",
    "k2r4/4P3/8/8/8/8/8/4K3 w - - 0 1",
    "1n1n4/2P5/8/8/8/8/8/K1k5 w - - 0 1",
    "4rkr1/4p1p1/8/8/8/8/8/4K2R w K - 0 1",
    "r3k3/8/8/8/8/8/8/R3K3 w Qq - 0 1",
    "r3k3/8/8/8/8/8/8/4K3 b q - 0 1",
    "3k4/8/8/8/8/8/8/R3K3 w Q - 0 1",
    "5k2/8/8/8/8/8/8/4K2R w K - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1",
    "rnbqkbnr/ppp1p1pp/8/3pPp2/8/8/PPPP1PPP/RNBQKBNR w KQkq f6 0 3",
    "rnbqkbnr/ppp2ppp/3p4/4p3/4P3/8/PPPP1PPP/RNBQKBNR w KQkq - 0 3",
    "4k3/8/8/2p1p3/3P4/8/8/4K3 w - - 0 1",
    "rnbqkbnr/pppp1ppp/8/4p3/6P1/5P2/PPPPP2P/RNBQKBNR b KQkq - 0 2",
    "4k3/8/8/8/1b6/8/3N4/4K1N1 w - - 0 1",
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    "8/2P1P3/8/8/8/k7/8/K7 w - - 0 1",
    "7k/8/5KQ1/8/8/8/8/8 w - - 0 1",
    "6k1/5ppp/8/8/8/8/8/R3R1K1 w - - 0 1",
    "4k3/8/8/8/4Pp2/8/8/4K3 b - e3 0 1",
    "2r1r3/8/8/8/8/k7/8/2R1R1K1 w - - 0 1",
    "B6B/8/8/8/8/k7/8/B5KB w - - 0 1",
    "7k/8/8/8/8/8/2Q3Q1/K1Q5 w - - 0 1",
};

const char* kStarts[] = {
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
    "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    "r2q1rk1/pP1p2pp/Q4n2/bbp1p3/Np6/1B3NBn/pPPP1PPP/R3K2R b KQ - 0 1",
    "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    "r3k2r/1b4bq/8/8/8/8/7B/R3K2R w KQkq - 0 1",
    "r3k2r/8/3Q4/8/8/5q2/8/R3K2R b KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1",
    "5k2/8/8/8/8/8/8/4K2R w K - 0 1",
    "3k4/8/8/8/8/8/8/R3K3 w Q - 0 1",
    "8/PPPP4/8/2k5/8/5K2/4pppp/8 w - - 0 1",
    "1n1n1n1n/PPPPPPPP/8/2k5/8/5K2/pppppppp/N1N1N1N1 w - - 0 1",
    "4k3/1P6/8/8/8/8/K7/8 w - - 0 1",
    "8/P1k5/K7/8/8/8/8/8 w - - 0 1",
    "2K2r2/4P3/8/8/8/8/8/3k4 w - - 0 1",
    "4k3/1p1p1p1p/8/P1P1P1P1/1p1p1p1p/8/P1P1P1P1/4K3 w - - 0 1",
    "4k3/p1p1p1p1/8/1P1P1P1P/p1p1p1p1/8/1P1P1P1P/4K3 b - - 0 1",
    "3k4/3p4/8/K1P4r/8/8/8/8 b - - 0 1",
    "8/8/4k3/8/2p5/8/B2P2K1/8 w - - 0 1",
    "8/8/1k6/2b5/2pP4/8/5K2/8 b - d3 0 1",
    "4k3/2p5/8/KP5r/8/8/8/8 b - - 0 1",
    "8/8/1P2K3/8/2n5/1q6/8/5k2 b - - 0 1",
    "4k3/8/8/KPp4r/8/8/8/8 w - c6 0 2",
    "r3k3/8/8/8/8/8/8/4K2R w KQkq - 0 1",
    "rnbqkbnr/ppp1pppp/8/3p4/8/8/PPPPPPPP/RNBQKBNR w KQkq d6 0 2",
    "8/8/8/4k3/8/8/8/KQ6 w - - 0 1",
    "8/8/3k4/8/8/8/8/R3K3 w Q - 0 1",
    "k7/8/8/8/8/8/8/1Q2K3 w - - 0 1",
    "7k/8/6K1/8/8/8/8/5Q2 w - - 0 1",
    "8/8/8/4k3/8/8/8/2B1KN2 w - - 0 1",
    "8/8/8/3k4/8/8/8/2B1KB2 w - - 0 1",
    "8/8/8/3k4/8/8/2p5/4K3 w - - 0 1",
    "8/8/8/3k4/8/2P5/8/4K3 b - - 0 1",
    "4k3/8/8/8/8/2b5/3n4/2B1K3 w - - 0 1",
    "4k3/8/8/8/8/8/3q4/4K3 w - - 0 1",
    "4k3/8/8/8/8/8/3n4/2B1K3 w - - 0 1",
    "4k3/8/8/8/8/8/3b4/1N2K3 w - - 0 1",
    "4k3/8/8/8/8/8/2b5/2B1K3 w - - 0 1",
    "rnb1kbnr/pppp1ppp/8/4p3/6Pq/5P2/PPPPP2P/RNBQKBNR w KQkq - 1 3",
    "7k/5Q2/6K1/8/8/8/8/8 b - - 0 1",
    "4k3/8/8/8/8/8/8/4KN2 b - - 0 1",
    "7k/8/6K1/8/8/8/8/R7 w - - 150 100",
    "7k/8/6K1/8/8/8/8/R7 w - - 149 100",
    "7k/8/6K1/8/8/8/8/R7 w - - 98 60",
    "4k3/2rn4/8/8/8/8/2RN4/4K3 w - - 0 1",
    "1n2k1n1/8/8/8/8/8/8/1N2K1N1 w - - 0 1",
    "r3k3/8/8/8/8/8/8/4K2R w - - 90 50",
    "4k3/8/8/8/8/8/8/R3K2r w Q - 120 80",
    "2kr4/8/8/8/8/8/8/2KR4 w - - 60 40",
};

}  // namespace

int main(int argc, char** argv) {
    if (argc != 3) {
        std::fprintf(stderr, "usage: %s <en.lang> <output.json>\n", argv[0]);
        return 2;
    }
    if (!loadEnglish(argv[1])) {
        std::fprintf(stderr, "cannot read %s\n", argv[1]);
        return 1;
    }

    std::string out = "{\n \"generator\": \"dedicated-server/tools/gen-chess-crosscheck.cpp (game rules: src/chess/position.cpp, game.cpp)\",\n";

    // FEN parsing / normalisation.
    out += " \"fens\": [\n";
    bool first = true;
    for (const char* f : kFens) {
        Position p;
        const bool ok = p.setFEN(f);
        out += std::string(first ? "" : ",\n") + "  [" + jstr(f) + ", " + (ok ? jstr(p.fen()) : std::string("null")) + "]";
        first = false;
    }
    out += "],\n";

    // SAN of every legal move of hand-picked positions.
    out += " \"san\": [\n";
    first = true;
    for (const char* f : kSanFens) {
        Position p;
        if (!p.setFEN(f)) {
            std::fprintf(stderr, "invalid SAN FEN: %s\n", f);
            return 1;
        }
        std::vector<Move> moves = p.legalMoves();
        std::sort(moves.begin(), moves.end(), [](const Move& a, const Move& b) { return u16(a) < u16(b); });
        std::string list;
        for (const Move& m : moves) list += std::string(list.empty() ? "" : ",") + "[" + std::to_string(u16(m)) + "," + jstr(p.toSAN(m)) + "]";
        out += std::string(first ? "" : ",\n") + "  {\"fen\": " + jstr(p.fen()) + ", \"moves\": [" + list + "]}";
        first = false;
    }
    out += "],\n";

    out += " \"pgnTags\": {\"event\": \"Crosscheck\", \"site\": \"Scacelith\", \"date\": \"2026.01.01\", \"round\": \"-\", \"white\": \"White\", "
           "\"black\": \"Black\", \"timeControl\": \"180+2\"},\n";

    // Games.
    std::vector<GameSpec> specs;
    const int kStandardGames = 120;
    for (int i = 0; i < kStandardGames; ++i) {
        Policy pol = Policy::Biased;
        if (i % 10 == 3) pol = Policy::Repetition;
        if (i % 10 == 7) pol = Policy::Uniform;
        specs.push_back({nullptr, pol, pol == Policy::Uniform ? 60 : 160, i < 12});
    }
    for (const char* s : kStarts) {
        const std::string f(s);
        const bool quiet = f.find(" 90 50") != std::string::npos || f.find(" 120 80") != std::string::npos ||
                           f.find(" 60 40") != std::string::npos || f.find("2rn4") != std::string::npos || f.find("1n2k1n1") != std::string::npos;
        const bool squeeze = f.find("KQ6") != std::string::npos || f.find("R3K3 w Q") != std::string::npos ||
                             f.find("1Q2K3") != std::string::npos || f.find("5Q2") != std::string::npos || f.find("2B1KN2") != std::string::npos ||
                             f.find("2B1KB2") != std::string::npos || f.find("2p5/4K3") != std::string::npos || f.find("2P5/8/4K3") != std::string::npos;
        if (quiet) {
            specs.push_back({s, Policy::Quiet, 260, true});
            specs.push_back({s, Policy::Repetition, 120, false});
        } else if (squeeze) {
            for (int k = 0; k < 4; ++k) specs.push_back({s, Policy::Squeeze, 80, k == 0});
        } else {
            for (int k = 0; k < 3; ++k) specs.push_back({s, k == 2 ? Policy::Repetition : Policy::Biased, 120, k == 0});
        }
    }

    Rng rng{0x5CAC0FFEE1234ULL};
    out += " \"games\": [\n";
    first = true;
    int count = 0;
    for (const GameSpec& spec : specs) {
        const std::string g = runGame(spec, rng);
        if (g.empty()) continue;
        out += std::string(first ? "" : ",\n") + g;
        first = false;
        ++count;
    }
    out += "]\n}\n";

    std::FILE* fp = std::fopen(argv[2], "wb");
    if (!fp) {
        std::fprintf(stderr, "cannot write %s\n", argv[2]);
        return 1;
    }
    std::fwrite(out.data(), 1, out.size(), fp);
    std::fclose(fp);
    std::fprintf(stderr, "%s: %d games, %zu bytes\n", argv[2], count, out.size());
    return 0;
}
