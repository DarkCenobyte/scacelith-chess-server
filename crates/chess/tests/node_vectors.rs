//! Reference vectors of the former Node.js server (tests/fixtures/node-vectors.json, written by
//! tests/fixtures/gen-node-vectors.mjs): the PGN reader on thousands of mutated texts and byte
//! strings (with default and lowered caps), lenient SAN on random and mutated strings, and the
//! PGN writer on random games with unicode tags, odd comments and every ending. The inputs are
//! regenerated here with the same generator; the games are drawn from `legal_moves`, so its
//! order is checked too.

mod common;

use common::{XorShift, fixture_path, read_json, repo_path};
use scacelith_chess::{
    ChessGame, Color, EndReason, GameStatus, PGN_LIMITS, PgnComment, PgnError, PgnGame, PgnLimits, PgnTags,
    Position, fnv1a32, move_uci, parse_san, read_pgn, read_pgn_bytes,
};
use serde_json::Value;

fn vectors() -> Value {
    read_json(fixture_path("node-vectors.json"))
}

/// A reader outcome as one line, as the generator writes it.
fn describe(outcome: Result<PgnGame, PgnError>) -> String {
    match outcome {
        Ok(g) => {
            let mut p =
                g.start_fen.as_deref().map_or_else(Position::start, |f| Position::from_fen(f).unwrap());
            let ucis: Vec<String> = g
                .moves
                .iter()
                .map(|&m| {
                    p.play(m).expect("every move read is legal");
                    move_uci(m)
                })
                .collect();
            let tags: Vec<String> = g.tags.iter().map(|(k, v)| format!("{k}\u{1}{v}")).collect();
            format!(
                "ok {} {} {} {}",
                g.result,
                g.start_fen.as_deref().unwrap_or("-"),
                ucis.join(","),
                tags.join("\u{2}")
            )
        }
        Err(e) => format!("err {}:{} {}", e.line, e.column, e.message),
    }
}

fn short_hash(line: &str) -> String {
    format!("{:04x}", fnv1a32(line.as_bytes()) & 0xffff)
}

/// The mutation of the generator: 1 to 6 deletions, insertions or replacements.
fn mutate<T: Copy>(items: &[T], alphabet: &[T], r: &mut XorShift) -> Vec<T> {
    let mut out = items.to_vec();
    let edits = 1 + r.below(6);
    for _ in 0..edits {
        let at = r.below(out.len() + 1);
        let c = alphabet[r.below(alphabet.len())];
        let op = r.next_f64();
        if op < 0.4 {
            if at < out.len() {
                out.remove(at);
            }
        } else if op < 0.7 {
            out.insert(at, c);
        } else if at < out.len() {
            out[at] = c;
        } else {
            out.push(c);
        }
    }
    out
}

fn limits_of(v: &Value) -> PgnLimits {
    if v.is_null() {
        return PGN_LIMITS;
    }
    let get = |k: &str| usize::try_from(v[k].as_u64().unwrap_or_else(|| panic!("limit {k}"))).unwrap();
    PgnLimits {
        max_bytes: get("maxBytes"),
        max_plies: get("maxPlies"),
        max_tags: get("maxTags"),
        max_tag_name: get("maxTagName"),
        max_tag_value: get("maxTagValue"),
        max_depth: get("maxDepth"),
        max_token: get("maxToken"),
    }
}

fn str_of(v: &Value) -> &str {
    v.as_str().unwrap_or_else(|| panic!("string expected: {v}"))
}

fn seed_of(v: &Value) -> u32 {
    u32::try_from(v.as_u64().unwrap()).unwrap()
}

#[test]
fn the_reader_gives_the_former_servers_outcome_on_mutated_input() {
    let vectors = vectors();
    let mut failures = Vec::new();
    for set in vectors["reader"].as_array().unwrap() {
        let name = str_of(&set["name"]);
        let limits = limits_of(&set["limits"]);
        let mut r = XorShift::new(seed_of(&set["seed"]));
        let count = usize::try_from(set["count"].as_u64().unwrap()).unwrap();
        let hashes = str_of(&set["hashes"]);
        assert_eq!(hashes.len(), count * 4);
        let samples: Vec<&str> = set["samples"].as_array().unwrap().iter().map(str_of).collect();
        let mut run = |n: usize, input: String, line: String| {
            if n < samples.len() && line != samples[n] {
                failures.push(format!("{name} #{n}: {line:?}\n  node: {:?}\n  input: {input:?}", samples[n]));
            } else if short_hash(&line) != hashes[n * 4..n * 4 + 4] {
                failures.push(format!("{name} #{n}: {line:?}\n  input: {input:?}"));
            }
            usize::from(line.starts_with("ok"))
        };
        let mut games = 0;
        if name == "bytes" {
            let to_bytes = |v: &Value| -> Vec<u8> {
                v.as_array().unwrap().iter().map(|b| u8::try_from(b.as_u64().unwrap()).unwrap()).collect()
            };
            let (base, alphabet) = (to_bytes(&set["base"]), to_bytes(&set["alphabet"]));
            for n in 0..count {
                let input = mutate(&base, &alphabet, &mut r);
                let line = describe(read_pgn_bytes(&input, &limits));
                games += run(n, format!("{input:?}"), line);
            }
        } else {
            let base = str_of(&set["base"]);
            let base: Vec<char> = if base.starts_with("server-pgn/") {
                std::fs::read_to_string(repo_path(&format!("tests/data/{base}"))).unwrap().chars().collect()
            } else {
                base.chars().collect()
            };
            let alphabet: Vec<char> = str_of(&set["alphabet"]).chars().collect();
            for n in 0..count {
                let input: String = mutate(&base, &alphabet, &mut r).into_iter().collect();
                let line = describe(read_pgn(&input, &limits));
                games += run(n, input, line);
            }
        }
        assert_eq!(u64::try_from(games).unwrap(), set["games"].as_u64().unwrap(), "{name}: games read");
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures[..failures.len().min(8)].join("\n")
    );
}

#[test]
fn the_unmodified_bases_read_the_same() {
    let vectors = vectors();
    let sets = vectors["reader"].as_array().unwrap();
    let bases: Vec<&str> = vectors["bases"].as_array().unwrap().iter().map(str_of).collect();
    for (set, want) in sets.iter().zip(bases) {
        let limits = limits_of(&set["limits"]);
        let got = match &set["base"] {
            Value::String(s) if s.starts_with("server-pgn/") => describe(read_pgn(
                &std::fs::read_to_string(repo_path(&format!("tests/data/{s}"))).unwrap(),
                &limits,
            )),
            Value::String(s) => describe(read_pgn(s, &limits)),
            Value::Array(a) => {
                let bytes: Vec<u8> = a.iter().map(|b| u8::try_from(b.as_u64().unwrap()).unwrap()).collect();
                describe(read_pgn_bytes(&bytes, &limits))
            }
            other => panic!("base {other}"),
        };
        assert_eq!(got, want, "{}", set["name"]);
    }
}

#[test]
fn lenient_san_matches_on_random_and_mutated_strings() {
    let vectors = vectors();
    let mut failures = Vec::new();
    let mut accepted = 0;
    for case in vectors["san"].as_array().unwrap() {
        let fen = str_of(&case["fen"]);
        let p = Position::from_fen(fen).unwrap();
        let alphabet: Vec<char> = str_of(&case["alphabet"]).chars().collect();
        let mut r = XorShift::new(seed_of(&case["seed"]));
        let forms: Vec<Vec<char>> = p
            .legal_moves()
            .into_iter()
            .flat_map(|m| [p.san(m).chars().collect(), move_uci(m).chars().collect()])
            .collect();
        for (n, want) in str_of(&case["results"]).split(' ').enumerate() {
            let text: String = if n % 2 == 0 {
                let len = 1 + r.below(8);
                (0..len).map(|_| alphabet[r.below(alphabet.len())]).collect()
            } else {
                mutate(&forms[r.below(forms.len())], &alphabet, &mut r).into_iter().collect()
            };
            let got = parse_san(&p, &text).map_or_else(|| "-".to_owned(), move_uci);
            if got != want {
                failures.push(format!("{text:?} in {fen}: {got}, node {want}"));
            }
            accepted += usize::from(got != "-");
        }
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures[..failures.len().min(8)].join("\n")
    );
    assert!(accepted > 100, "{accepted} accepted");
}

fn comment_of(v: &Value) -> Option<PgnComment> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(PgnComment::Text(s.clone())),
        Value::Array(words) => Some(PgnComment::Words(words.iter().map(|w| str_of(w).to_owned()).collect())),
        other => panic!("comment {other}"),
    }
}

fn tags_of(v: &Value, comments: Vec<Option<PgnComment>>) -> PgnTags {
    let opt = |k: &str| v.get(k).map(|s| str_of(s).to_owned());
    let pairs = |k: &str| -> Vec<(String, String)> {
        v.get(k).map_or_else(Vec::new, |a| {
            a.as_array()
                .unwrap()
                .iter()
                .map(|p| (str_of(&p[0]).to_owned(), str_of(&p[1]).to_owned()))
                .collect()
        })
    };
    PgnTags {
        event: opt("event"),
        site: opt("site"),
        date: opt("date"),
        round: opt("round"),
        white: opt("white"),
        black: opt("black"),
        time_control: opt("timeControl"),
        after_result: pairs("afterResult"),
        extra: pairs("extra"),
        comments,
    }
}

#[test]
fn the_writer_and_the_move_order_match_on_random_games() {
    let vectors = vectors();
    let comments_table: Vec<Option<PgnComment>> =
        vectors["comments"].as_array().unwrap().iter().map(comment_of).collect();
    let tag_sets = vectors["tagSets"].as_array().unwrap();
    let mut r = XorShift::new(99);
    let mut full_texts = 0;
    for (n, want) in vectors["writer"].as_array().unwrap().iter().enumerate() {
        let start = want["start"].as_str();
        let mut g = ChessGame::new(start).unwrap();
        let plies = r.below(120);
        while g.ply() < plies && !g.is_over() {
            let legal = g.position().legal_moves();
            g.play(legal[r.below(legal.len())]).unwrap();
        }
        let ucis = g.uci_moves().join(" ");
        let node_ucis = str_of(&want["uci"]);
        if ucis != node_ucis {
            let first = ucis.split(' ').zip(node_ucis.split(' ')).position(|(a, b)| a != b).unwrap_or(0);
            let before = ChessGame::from_moves(start, &g.moves()[..first]).unwrap();
            panic!(
                "game {n}: the legal move order differs at ply {first} in {}\n  rust: {ucis}\n  node: {node_ucis}",
                before.position().fen()
            );
        }
        let side = g.position().side();
        if !g.is_over() {
            match str_of(&want["action"]) {
                "resign" => assert!(g.resign(side)),
                "agreeDraw" => assert!(g.agree_draw()),
                "abort" => assert_eq!(g.end(GameStatus::Aborted, EndReason::Aborted), Ok(true)),
                "flagFall" => assert!(g.flag_fall(side)),
                "abandon" => assert_eq!(g.end(GameStatus::WhiteWins, EndReason::Abandonment), Ok(true)),
                "claim" => {
                    g.claim_draw();
                }
                "none" => {}
                other => panic!("action {other}"),
            }
        }
        assert_eq!(u64::from(g.status().as_u8()), want["status"].as_u64().unwrap(), "game {n}: status");
        assert_eq!(u64::from(g.reason().as_u8()), want["reason"].as_u64().unwrap(), "game {n}: reason");
        let comments = (0..g.ply()).map(|i| comments_table[(i + n) % comments_table.len()].clone()).collect();
        let tags = tags_of(&tag_sets[n % tag_sets.len()], comments);
        let text = g.pgn(&tags);
        if let Some(node_text) = want["pgn"].as_str() {
            assert_eq!(text, node_text, "game {n}");
            full_texts += 1;
        }
        assert_eq!(
            format!("{:08x}", fnv1a32(text.as_bytes())),
            str_of(&want["pgnHash"]),
            "game {n}:\n{text}"
        );
        // What the writer wrote reads back (comments and tags included).
        let back = read_pgn(&text, &PGN_LIMITS)
            .unwrap_or_else(|e| panic!("game {n}: {e} at {}:{}\n{text}", e.line, e.column));
        assert_eq!(back.moves, g.moves(), "game {n}");
        assert_eq!(back.result, g.result_string());
    }
    assert!(full_texts >= 6);
    assert_eq!(Color::White.opposite(), Color::Black);
}
