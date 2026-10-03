//! Players of the black-box tests: an account on the real server (HTTPS API) and a realtime
//! connection (SDK) that keeps every message it received, so that a test waits for a message
//! received since a mark ([`Client::mark`], [`wait_msg!`](crate::wait_msg)), plus a local copy of
//! the rules for each move's `posHash` ([`Table`]).

use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;

use scacelith_chess::ChessGame;
use scacelith_client::{ApiClient, ClientError, CloseInfo, ConnectOptions, Connection, Endpoint, Login};
use scacelith_protocol::{
    ChallengeAccept, ChallengeCreate, ClientMsg, ColorPref, DrawAnswer, DrawOffer, GameSnapshot, Move,
    MoveMade, QueueJoin, Resign, ServerMsg, Welcome, uci_to_move,
};

use super::server::TestServer;

/// The password of every test account.
pub const PASSWORD: &str = "correct horse battery staple 9";
/// The usual wait for a message.
pub const WAIT: Duration = Duration::from_secs(10);

/// Waits for a message received since `$since` that matches the pattern (with an optional
/// guard), within `$limit` (default [`WAIT`]), and evaluates to `$out`; panics on a timeout or
/// when the connection ends first.
///
/// ```ignore
/// let end = wait_msg!(alice.client, mark, ServerMsg::GameEnd(e) if e.game == id => e.clone());
/// ```
#[macro_export]
macro_rules! wait_msg {
    ($client:expr, $since:expr, $limit:expr, $pat:pat $(if $guard:expr)? => $out:expr) => {
        $client
            .wait_for($since, $limit, stringify!($pat), |m| match m {
                $pat $(if $guard)? => Some($out),
                _ => None,
            })
            .await
    };
    ($client:expr, $since:expr, $pat:pat $(if $guard:expr)? => $out:expr) => {
        $crate::wait_msg!($client, $since, $crate::support::players::WAIT, $pat $(if $guard)? => $out)
    };
}

/// A realtime connection that keeps the messages it received.
#[derive(Debug)]
pub struct Client {
    conn: Connection,
    history: Vec<ServerMsg>,
    closed: Option<CloseInfo>,
}

impl Client {
    /// Connects to `endpoint` with the session token `token` (Welcome received).
    pub async fn connect(endpoint: &Endpoint, token: &str) -> Result<Client, ClientError> {
        let conn = Connection::connect(endpoint, token, &ConnectOptions::default()).await?;
        Ok(Client { conn, history: Vec::new(), closed: None })
    }

    /// The `Welcome` of the connection.
    pub fn welcome(&self) -> &Welcome {
        self.conn.welcome()
    }

    /// The account of the connection.
    pub fn user_id(&self) -> u32 {
        self.conn.user_id()
    }

    /// The SDK connection (raw frames, the session).
    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    /// A mark: the messages received after it are those a wait `since` it looks at.
    pub fn mark(&self) -> usize {
        self.history.len()
    }

    /// Every message received so far.
    pub fn history(&self) -> &[ServerMsg] {
        &self.history
    }

    /// Sends a message (numbered); returns its `seq`.
    pub fn send(&self, msg: impl Into<ClientMsg>) -> u32 {
        self.conn.send(msg).expect("message sent")
    }

    /// Receives one more message into the history; `false` once the connection has ended.
    async fn pull(&mut self) -> bool {
        if self.closed.is_some() {
            return false;
        }
        match self.conn.recv().await {
            Ok(msg) => {
                self.history.push(msg);
                true
            }
            Err(ClientError::Closed(info)) => {
                self.closed = Some(info);
                false
            }
            Err(e) => panic!("receive failed: {e}"),
        }
    }

    /// The first message received since `since` for which `pick` gives a value, waiting for at
    /// most `limit`; `None` on a timeout or when the connection ended first.
    pub async fn try_wait_for<T>(
        &mut self,
        since: usize,
        limit: Duration,
        mut pick: impl FnMut(&ServerMsg) -> Option<T>,
    ) -> Option<T> {
        let deadline = tokio::time::Instant::now() + limit;
        let mut next = since;
        loop {
            while next < self.history.len() {
                if let Some(found) = pick(&self.history[next]) {
                    return Some(found);
                }
                next += 1;
            }
            match tokio::time::timeout_at(deadline, self.pull()).await {
                Ok(true) => {}
                Ok(false) | Err(_) => return None,
            }
        }
    }

    /// [`Client::try_wait_for`] that panics, naming `what`, on a timeout or a closed connection.
    pub async fn wait_for<T>(
        &mut self,
        since: usize,
        limit: Duration,
        what: &str,
        pick: impl FnMut(&ServerMsg) -> Option<T>,
    ) -> T {
        match self.try_wait_for(since, limit, pick).await {
            Some(found) => found,
            None => panic!(
                "{} did not receive {what} within {limit:?} (closed: {:?}); received since the mark: {:?}",
                self.welcome().username,
                self.closed,
                &self.history[since.min(self.history.len())..]
            ),
        }
    }

    /// Waits until the connection has ended (keeping what arrives meanwhile) and returns how.
    pub async fn closed(&mut self, limit: Duration) -> CloseInfo {
        let ended = tokio::time::timeout(limit, async { while self.pull().await {} }).await;
        assert!(ended.is_ok(), "{}: the connection did not close within {limit:?}", self.welcome().username);
        self.closed.clone().expect("closed")
    }

    /// How the connection ended, when it has (as far as this client read).
    pub fn close_info(&self) -> Option<&CloseInfo> {
        self.closed.as_ref()
    }

    /// Whether the connection is still open (nothing read says otherwise).
    pub fn is_open(&self) -> bool {
        self.closed.is_none() && self.conn.close_info().is_none()
    }

    /// Closes the connection (1000) and waits for its end.
    pub async fn close(&mut self) {
        if self.is_open() {
            self.conn.close();
            let _ = tokio::time::timeout(Duration::from_secs(5), async { while self.pull().await {} }).await;
        }
    }

    /// The moves of game `game` as this client knows them: those of its last snapshot, then the
    /// `MoveMade` that followed it in order.
    pub fn moves_of(&self, game: u64) -> Vec<u16> {
        let mut moves: Vec<u16> = Vec::new();
        for msg in &self.history {
            match msg {
                ServerMsg::GameSnapshot(s) if s.game == game => {
                    moves = s.moves.iter().map(|m| m.r#move).collect();
                }
                ServerMsg::MoveMade(m) if m.game == game && usize::from(m.ply) == moves.len() => {
                    moves.push(m.r#move);
                }
                _ => {}
            }
        }
        moves
    }

    /// `ChallengeCreate`.
    pub fn challenge(&self, target: &str, base_sec: u16, inc_sec: u8, rated: bool, color: ColorPref) -> u32 {
        self.send(ChallengeCreate { seq: 0, target: target.into(), base_sec, inc_sec, rated, color })
    }

    /// `ChallengeAccept`.
    pub fn accept_challenge(&self, id: u32) -> u32 {
        self.send(ChallengeAccept { seq: 0, id })
    }

    /// `QueueJoin`.
    pub fn join_queue(&self, category: &str, rated: bool) -> u32 {
        self.send(QueueJoin { seq: 0, category: category.into(), rated })
    }

    /// `Resign`.
    pub fn resign(&self, game: u64) -> u32 {
        self.send(Resign { seq: 0, game })
    }

    /// `DrawOffer`.
    pub fn offer_draw(&self, game: u64) -> u32 {
        self.send(DrawOffer { seq: 0, game })
    }

    /// `DrawAnswer`.
    pub fn answer_draw(&self, game: u64, accept: bool) -> u32 {
        self.send(DrawAnswer { seq: 0, game, accept })
    }

    /// A `Move` intent.
    pub fn send_move(
        &self,
        game: u64,
        ply: u16,
        mv: u16,
        pos_hash: u32,
        think_ms: u32,
        draw_offer: bool,
    ) -> u32 {
        self.send(Move { seq: 0, game, ply, r#move: mv, pos_hash, think_ms, draw_offer })
    }
}

/// An account: its API client, session token, name and id.
#[derive(Debug)]
pub struct Account {
    pub api: ApiClient,
    pub token: String,
    pub name: String,
    pub user_id: u32,
    pub email: String,
}

/// Signs `name` in (password [`PASSWORD`]) and returns its session.
pub async fn sign_in(srv: &TestServer, name: &str, label: Option<&str>) -> Account {
    let api = srv.api();
    let login = api.login(name, PASSWORD, label).await.unwrap_or_else(|e| panic!("login {name}: {e}"));
    let Login::Session(session) = login else { panic!("login {name}: a second factor is asked") };
    let user_id = session.user["id"].as_u64().expect("the account id") as u32;
    let email = session.user["email"].as_str().unwrap_or_default().to_string();
    Account { api, token: session.token, name: name.to_string(), user_id, email }
}

/// Registers `name` (e-mail `<name>@example.org`), signs it in and returns the account.
pub async fn account(srv: &TestServer, name: &str) -> Account {
    let api = srv.api();
    let answer = api
        .register(name, &format!("{name}@example.org"), PASSWORD)
        .await
        .unwrap_or_else(|e| panic!("register {name}: {e}"));
    assert!(
        matches!(answer["status"].as_str(), Some("ready" | "verification_sent")),
        "register {name}: {answer}"
    );
    sign_in(srv, name, None).await
}

/// A realtime connection with `token`, from 127.0.0.1.
pub async fn connect(srv: &TestServer, token: &str) -> Result<Client, ClientError> {
    Client::connect(&srv.endpoint(), token).await
}

/// A registered, signed-in and connected player.
#[derive(Debug)]
pub struct Player {
    pub acc: Account,
    pub client: Client,
}

impl Player {
    /// The account name.
    pub fn name(&self) -> &str {
        &self.acc.name
    }

    /// The session token.
    pub fn token(&self) -> &str {
        &self.acc.token
    }

    /// Connects again (a new connection replaces the client).
    pub async fn reconnect(&mut self, srv: &TestServer) {
        self.client = connect(srv, &self.acc.token).await.expect("connected again");
    }
}

/// [`account`] then a connection.
pub async fn player(srv: &TestServer, name: &str) -> Player {
    let acc = account(srv, name).await;
    let client = connect(srv, &acc.token).await.unwrap_or_else(|e| panic!("connect {name}: {e}"));
    Player { acc, client }
}

/// Several players made together (their password hashes run side by side).
pub async fn players<const N: usize>(srv: &TestServer, names: [&str; N]) -> [Player; N] {
    let made = join_all(names.iter().map(|n| player(srv, n))).await;
    made.try_into().unwrap_or_else(|_| unreachable!("one player per name"))
}

/// Several accounts made together.
pub async fn accounts<const N: usize>(srv: &TestServer, names: [&str; N]) -> [Account; N] {
    let made = join_all(names.iter().map(|n| account(srv, n))).await;
    made.try_into().unwrap_or_else(|_| unreachable!("one account per name"))
}

/// Runs the futures concurrently on the current task and returns their outputs in order.
pub async fn join_all<F: Future>(futs: impl IntoIterator<Item = F>) -> Vec<F::Output> {
    let mut futs: Vec<Pin<Box<F>>> = futs.into_iter().map(Box::pin).collect();
    let mut out: Vec<Option<F::Output>> = futs.iter().map(|_| None).collect();
    std::future::poll_fn(|cx| {
        let mut pending = false;
        for (fut, slot) in futs.iter_mut().zip(out.iter_mut()) {
            if slot.is_none() {
                match fut.as_mut().poll(cx) {
                    Poll::Ready(v) => *slot = Some(v),
                    Poll::Pending => pending = true,
                }
            }
        }
        if pending { Poll::Pending } else { Poll::Ready(()) }
    })
    .await;
    out.into_iter().map(|v| v.expect("every future is ready")).collect()
}

/// A game started with a direct challenge: `a` challenges `b` (White asked for `a`), `b` accepts.
/// Returns the game id; `a` plays White.
pub async fn challenge_game(a: &mut Player, b: &mut Player, base_sec: u16, inc_sec: u8, rated: bool) -> u64 {
    let (ma, mb) = (a.client.mark(), b.client.mark());
    a.client.challenge(b.name(), base_sec, inc_sec, rated, ColorPref::White);
    let id = wait_msg!(b.client, mb, ServerMsg::ChallengeReceived(r) => r.id);
    b.client.accept_challenge(id);
    let sa: GameSnapshot = wait_msg!(a.client, ma, ServerMsg::GameSnapshot(s) => s.clone());
    let sb: GameSnapshot =
        wait_msg!(b.client, mb, ServerMsg::GameSnapshot(s) if s.game == sa.game => s.clone());
    assert_eq!(sa.you, scacelith_protocol::Color::White);
    assert_eq!(sb.you, scacelith_protocol::Color::Black);
    sa.game
}

/// Both players queue in `category` (rated) and get paired with each other. Returns the game id
/// and whether `a` plays White, with both snapshots.
pub async fn queue_game(
    a: &mut Player,
    b: &mut Player,
    category: &str,
) -> (u64, bool, GameSnapshot, GameSnapshot) {
    let (ma, mb) = (a.client.mark(), b.client.mark());
    a.client.join_queue(category, true);
    b.client.join_queue(category, true);
    let limit = Duration::from_secs(20);
    let sa: GameSnapshot = wait_msg!(a.client, ma, limit, ServerMsg::GameSnapshot(s) => s.clone());
    let sb: GameSnapshot =
        wait_msg!(b.client, mb, limit, ServerMsg::GameSnapshot(s) if s.game == sa.game => s.clone());
    (sa.game, sa.you == scacelith_protocol::Color::White, sa, sb)
}

/// The UCI move as the protocol's u16.
pub fn mv(uci: &str) -> u16 {
    uci_to_move(uci).unwrap_or_else(|| panic!("not a move: {uci}"))
}

/// The local mirror of a game (for `posHash`), played through the server.
#[derive(Debug)]
pub struct Table {
    pub id: u64,
    pub rules: ChessGame,
}

impl Table {
    /// The mirror of game `id` at its start.
    pub fn new(id: u64) -> Table {
        Table { id, rules: ChessGame::default() }
    }

    /// Plies played.
    pub fn ply(&self) -> u16 {
        self.rules.ply() as u16
    }

    /// `posHash` of the current position.
    pub fn hash(&self) -> u32 {
        self.rules.position().digest()
    }

    /// Plays `uci` in the mirror only (a move the server accepted).
    pub fn apply(&mut self, uci: &str) {
        let m = self.rules.position().parse_uci(uci).unwrap_or_else(|| panic!("not a legal move: {uci}"));
        self.rules.play(m).unwrap_or_else(|e| panic!("local rules refused {uci}: {e:?}"));
    }

    /// Sends `uci` from `by` for the current ply with the current position's hash (the mirror is
    /// not updated); returns the seq.
    pub fn send(&self, by: &Player, uci: &str) -> u32 {
        by.client.send_move(self.id, self.ply(), mv(uci), self.hash(), 500, false)
    }

    /// Plays one move for the side to move (`white` or `black`) and waits until both players
    /// received its `MoveMade`; returns the mover's `MoveMade`.
    pub async fn play(&mut self, white: &mut Player, black: &mut Player, uci: &str) -> MoveMade {
        self.play_with(white, black, uci, 500).await
    }

    /// [`Table::play`] with the `thinkMs` the client claims.
    pub async fn play_with(
        &mut self,
        white: &mut Player,
        black: &mut Player,
        uci: &str,
        think_ms: u32,
    ) -> MoveMade {
        let ply = self.ply();
        let (mover, other) = if ply.is_multiple_of(2) { (white, black) } else { (black, white) };
        let (mm, mo) = (mover.client.mark(), other.client.mark());
        mover.client.send_move(self.id, ply, mv(uci), self.hash(), think_ms, false);
        let id = self.id;
        let made =
            wait_msg!(mover.client, mm, ServerMsg::MoveMade(m) if m.game == id && m.ply == ply => m.clone());
        wait_msg!(other.client, mo, ServerMsg::MoveMade(m) if m.game == id && m.ply == ply => ());
        self.apply(uci);
        made
    }

    /// Plays the moves in order.
    pub async fn play_all(
        &mut self,
        white: &mut Player,
        black: &mut Player,
        moves: &[&str],
    ) -> Option<MoveMade> {
        let mut last = None;
        for uci in moves {
            last = Some(self.play(white, black, uci).await);
        }
        last
    }
}

/// Closes the connections of the players.
pub async fn close_all(players: &mut [&mut Player]) {
    for p in players.iter_mut() {
        p.client.close().await;
    }
}
