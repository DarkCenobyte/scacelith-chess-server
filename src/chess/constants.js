// Enumerations shared with the protocol. The values are those of src/protocol/schema.js
// (enums.GameStatus, enums.EndReason, MoveFlag), which mirror chess::GameStatus /
// chess::GameEndReason / chess::MoveFlags of the game; test/unit/chess.api.test.js checks that
// they stay equal. They are repeated here so that the rules module has no dependency.

/** Colours. */
export const WHITE = 0;
export const BLACK = 1;

/** chess::PieceType numbering (also the promotion field of a u16 move). */
export const PieceType = Object.freeze({ None: 0, Pawn: 1, Knight: 2, Bishop: 3, Rook: 4, Queen: 5, King: 6 });

/** enums.GameStatus of the protocol. */
export const GameStatus = Object.freeze({ Ongoing: 0, WhiteWins: 1, BlackWins: 2, Draw: 3, Aborted: 4 });

/** enums.EndReason of the protocol (0..13 = chess::GameEndReason, 20+ online only). */
export const EndReason = Object.freeze({
    None: 0, Checkmate: 1, Resignation: 2, Timeout: 3, IllegalMoves: 4, Stalemate: 5,
    InsufficientMaterial: 6, TimeoutVsInsufficient: 7, FivefoldRepetition: 8, SeventyFiveMoves: 9,
    ThreefoldClaim: 10, FiftyMoveClaim: 11, Agreement: 12, IllegalMovesVsInsufficient: 13,
    Abandonment: 20, AbandonmentVsInsufficient: 21, Aborted: 22, NoShow: 23, Forfeit: 24,
    ServerAborted: 25, BothDisconnected: 26,
});

/** MoveFlag bits returned by play() (the protocol's MoveMade.flags). */
export const MoveFlag = Object.freeze({
    Capture: 1, EnPassant: 2, CastleKing: 4, CastleQueen: 8, DoublePush: 16, Promotion: 32, Check: 64, Mate: 128,
});

/** Castling rights bits of Position.castling (chess::CastlingRights). */
export const CastlingRight = Object.freeze({ WhiteKingSide: 1, WhiteQueenSide: 2, BlackKingSide: 4, BlackQueenSide: 8 });
