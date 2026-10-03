# cburnett chess pieces

The twelve SVG files of this folder (`wK.svg` ... `bP.svg`: w/b = White/Black, K Q R B N P =
king, queen, rook, bishop, knight, pawn) are the "cburnett" chess piece set by
**Colin M.L. Burnett** (https://en.wikipedia.org/wiki/User:Cburnett), licensed under the
**GNU General Public License, version 2 or (at your option) any later version** (GPLv2+,
https://www.gnu.org/licenses/gpl-2.0.txt).

Source: the copies distributed with lichess (https://github.com/lichess-org/lila), downloaded on
2026-10-01 from
`https://raw.githubusercontent.com/lichess-org/lila/master/public/piece/cburnett/<name>.svg`,
unchanged. lichess lists the set, its author and its licence in its `COPYING.md`:

> public/piece/cburnett | Colin M.L. Burnett | GPLv2+

The dedicated server (GPL-3.0-or-later, see `dedicated-server/Cargo.toml`) embeds them and
rasterizes them in `crates/gif/src/pieces.rs` to draw the boards of the animated GIFs of games.
GPLv2+ material may be combined with GPL-3.0-or-later code; the combination is distributed under
the GPL version 3 or later.
