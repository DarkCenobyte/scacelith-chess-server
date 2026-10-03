//! A fake OpenID Connect provider (Google-like) on 127.0.0.1: the token endpoint (authorization
//! code, PKCE S256, client secret, and the redirect URI of the code's authorization request byte
//! for byte), the JWKS endpoint with a `Cache-Control`, RS256 ID tokens signed with fixed test
//! keys. The browser step is [`FakeOidc::authorize`]: it reads the authorization URL the server
//! built and returns the query Google would send the browser back with.

use std::collections::{BTreeMap, HashMap};
use std::convert::Infallible;
use std::sync::Arc;

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::rsa::PublicKeyComponents;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

use super::super::OidcOptions;
use super::super::oidc::{OidcEndpoints, check_redirect_uri, pkce_challenge};
use crate::clock::{Clock, ManualClock};
use crate::http::url::parse_urlencoded;
use crate::security::encoding::{b64_url, node_b64_decode};
use crate::security::keys::random_token;

/// The issuer of the ID tokens.
pub(crate) const ISSUER: &str = "https://accounts.google.com";

/// RSA keys (PKCS#8, base64) made for these tests only: 0 signs, 1 replaces it in a key
/// rotation, 2 forges signatures.
const TEST_KEYS: [&str; 3] = [
    "MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDDfrUy5G+HSAoEbjhXjcDbKj/dawXfiLpV2NbBJpDjKd7/Gdr/\
     PZnN2sfcSxfIgr1se1kTj6Rh2lwhHJUJQt2ISszH1cVRK41LmXJuZfakimR2ovT1m8IkYjk5pYlgu1MoLbCbvDpo0Lva0JWMgw+Q\
     l3/QWQp5ZFjTvyv05Blt/INx/PkSUmGkC5HMbxdQGuNs8poJCsBkGaVr7zs15GoJ0OGzFm3IHsbVHDPy0uN04c7Dhe4MeGaO13QF\
     SuMYEFpRu+r/6Mv0pjJJgYGrqS4FX8c0fBXe6DnWK7/Rq5+Ki4izyo9cN5Zuqm/zBFqTAhqOg5HmodJCca8ljl7iOgXBAgMBAAEC\
     ggEADivtw7+GjDZ/L2deuI4YmGqoJYtqv2kVz5UAAGbPeFWNNPicBQZjU2ZpjZZ4phBAM2XVBRN+dZooNT2cBNyu2tXyoVvlnrw6\
     /8DNvaNbl/2bnx6yBj9Lzr+7aT/kRQ8J+K7TOimQzGSxRtakY1gO3/WeONgePRg2o5Gcr0x9FsQSXu58qrtUrsCXJlpXhB2g7Seg\
     152kUpKkJS0eDWdp3221kEua+6TS7v6INkhOc1V2wxUJX8w0mx2Fy/18PKiWV1k1wUQbiFuaVYqGjIqrcgfmEo8ENLhzqPPzLSyZ\
     vXaXf2LmLM9DONynD9MgMv+soSN7YjSsll5y1n5prwuEJwKBgQDtnvOOt5mhzC9SJwkQ+g5ZjFMp3WN9lz/1SSbayznULDQ7j29e\
     jFzwyvqMRswDDzuAj62eEuiiZiuqMcKcVIquLYRsQjuOLwlZneZCMawxEBt2oRR9QkIJYy82mNjaMFNZXemGDJq/QTFhkz+OUxBl\
     3Juy2rP4Sp65bOIFOFFKHwKBgQDSnaKY1ixIt2E3QIRJ1GlBQr61skJ4ymjU3lfeqzIgvtthf4bf/59j5wJT2G+ypBJEuacpkpLT\
     Lz9CWVmnRgYZWjmb5zPnCVSH7KmagMr+BBf9EJx/SoBlKcAdWtrburXLKfWJUnsdmVcKai0sH3nij7fwahvF2Tu17uwWQ7R0HwKB\
     gFKglCbLdzPI8aeKhV+O5FCFOCH1pvP0FTxw/H7Wbjx2Ro9zeBGTk3nzyx3ePHDP6ivxSjkOcDCQgJyFAxwjVbntf/+5JEZz2rd7\
     7aaU6UCCTlp49sv7r0TeZXuBuN5eMY2A14RSe7kHrWk1r8MI5UnWZZnS7QPoxrrJvup7w+CXAoGAdoBvHeNTY06aikXoqMm0tx1g\
     xEaaE/B+71Zyxjw4pif8s2zXbG0dN06hBp/+qNqb1MNIhKGNrvkkdKZlRTKm99jGFSwPDe03/IpyGxZgIHAZNzADfbNjbogBKgMW\
     pQ72fmsLVcpsrwi56og3Bl5na8xFSCXLnpRNfl4Bw70waS0CgYBGMveRVhIvqiQfV9woARkYpHIGXHDVvhEUtKSZchNfXwaIjmJK\
     EIUrgLB5MCbehKGSvU0iDzKYyRuSd/HsTtNpWhX7WayicMVeyCOzhhDWIRmuElxLPihI2sf/2IOPfJQjjIdbfY6aALkV2h5svd+s\
     L5wWKqGdsBTFS+i3LyTFWg==",
    "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDAT31uWPM0dv+5u7DcWl/frUyD/Ob48aY9rw2Ae1VmfK/tdThH\
     jfRy8Nm4l6LLDbzTe+ipGuXPPtc0uCf2ti96DSJaG79Ff4OZdsXjzrI2Bs3xoi3woMyO9p8MKRUXpnRpXu5OcyKDycJR137BMSH0\
     KGN44EiYRPdBtzBDOIqHZxN/arpqJN+gzMxtYI6RinMSLJzkyoKoc8HlxobmhtmsBux53vRcrLgp6g0gTF/N+umQovpElaN82GBN\
     QNXYIeWgLMINlODDifNiRIuCBmES5MpYLsWc2hrL4Qz5k9EucJw8FrYNP8akzvMeDNHLoKny8p2h47QizOnJgLP9Pl1ZAgMBAAEC\
     ggEAL2JmsY3RPxgjIoyQo3rRLLfypoLmFmjUYVaEqSe6fyox3vyHBXqAxOF7g/Q49HdKE4HwbdpmzY3aUO8fSbbSM+yQjktP8bvd\
     nS6ay+reFlnO3L7tOuEkBgXWYpSs0cr93Ai8BzBvTLGl46yJWujUSOi6ucnpmTtFATg1+BoyZ/IcGNkSJqqpQJ7TCa38rIQ1Wjlg\
     mfrXe94K9SFAQFpl6EBXRoXxVa4YaKBM0mJp9RhpfNJeHY5oaUB7wpJXV5vLDzaLFm+6H4/jEKWDtAdoP5xC3o9h6960k818Pue4\
     s3abfLwierE4T4/kuhQTrlLVUebjVPTJp6EqbSa0cGe90QKBgQDzUjVESYbNN1K+93DVP6Myb/pbjmzUO8tKIWrOjuff7rOhr9s0\
     qAD58yB4U8k9IGmIwPx2TIC36pdafVRXtk2yZnSl7B25Dt2L7+7EqF39OCbpVYXjsmMAAcSyG3Cpi/uy9UBgl6EeL/rlG0NAZFIo\
     txnu37OLr89ABVZnL8mQwwKBgQDKVNLeks/qiU8gikagW9hiVLmpWHeoW3Lm8aHVXDVK/loVuucm0mu5y5uFKBfl309S527KvVpD\
     Z3/JNDXlJeasrYzTUQtX6UZUinyEThlTMTrDmvFreLEOs4C81dSnvh2rxq9Imulf9mD0bIxYnEfJ5NcDq+2YjrijAfmzOrb3swKB\
     gQDF3MreDeBljqBmFDcX4hjmkfKHc0kCSOFmFciR0dmy2pwOVj+uERRCLTfxQUj6wRmwkCZ8WHevlz+e4R2t+dwyv2gJ6Pi+nN0B\
     x4llN/i+SmQyCE7JOy2QOt/labTy2pdCDndcVNE7CA9BMSs9+JZq74JSIm0RoVSqHe0GGfESGwKBgFoqBLJk8DyPJfDVTfXmF/5x\
     zS7XGrOu1Pxvj3O6HJGn2VM3sAyP1qu8PTjQjh3FLt89/RRh31iMRdjO6HmOM0aeLFR7GDf733iLIkP/Xa/CD3LdmFoRNa3cdjEw\
     hQyfXy+OYcxUJE28SKaDvOM7+Y3R6bNcKYxDhao2liS1tJWbAoGAHgq437hUlY1xVlLG9dDIXB4aM6NPbdxj2Pa7h54+5Mrmoghk\
     5trCufBDSvrgB2Lt7BqnHXfCP6n9d8MGNjFeN1mXrPAPDeGPKpoW9/RgneOh0bv9wlYJZIwcDrQjxgO++sauXnFDbWDQfL/7Y7YZ\
     7zDTqbuhelWbncdRPlVZNSY=",
    "MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCuhnkGHeDcNEALoL4wHB8xBoHje6MopTbizxGSBQcycmOcMaLa\
     +XqNtouX0HXvP1nYc7dc1sYmX5k16sJQ+QyC5EQoHp1arm8VgI2UKVvVCj0jwUgSICXxTzC3ZJ6i1c8JirnzWk8tFb1FB+4ngJbx\
     f7o/+bCCrGnyV2qmLhzufJ/CrgEFRs8pFDp4l69GcUgF5qC1RkxUpGkR9/GtB2O3hmDqxgD4y93ZmE7IVnyBZYMAB8AC7BZyWmTB\
     33PxunKXYfdVk0jOn86ohAfS6YvjB9mGEbIA3khH58lrZbvlJ5FmBcGlbYUdBczLhXNLmlQqXRzS4Oem8vZRJYdxLrwhAgMBAAEC\
     ggEAT+91hRCwSguAfhLsqVpoCuthGJErZNVvMykGcivtZPSxnPu7DLKRqFDA1RL8wUzx2Mr/Y/1XGoJUyTYyNCYtzdHBSeLjzYHx\
     jRapQgBDeGtNH5BKc0rYNhNAS+9BHXHydvtcOhLWCt9XJkQAl4U1HX2UD/NMHj3b9MyHI4AhbjVSt/o+qoGOmeN91MR2VluroRWn\
     SAVqIzZ/KoxtC6Jzlz9TWBPQoRgdhMqLUwrihEt0O+xwsE/nAooe6qMIU1jcxWomcHAqLtcoZV/qXKtMWMxR9SAgVLZp7yFtDo/1\
     Jn/rPAxnCHakIi8N2+6jJXru8PlPSrCnwYB8HP4Pt/bRlQKBgQDaa3X3Vyrc4xSsXqCymjfqhijaLYzMv16vABbuZx1QEodrvYG+\
     kpzh0PNQ4eXHPZgyY8TwnrDPtQJtdp9NZYEuUov+p6w9l6MNuxFVus9jX8mRJFL84OvDqMA35cq/zUfcKtKluJSqbGtqQc9TjkbF\
     YzGgfKogxEADYBnHMROkUwKBgQDMjaGMA8jaL2WeN9Q/4P1mxYTjJWTCAZiRx+OgP4fi3TMP0y/wPrCyoTgFrFNFfSF+xZ5TAWXz\
     QBfLLZbCAIRISJA+XoOazyIDO2m1eZgofqqUaNtkvIzCTb84fATtoAFUXWS8WPCpJjfP6FS19aXfCDBjJvDhzNaisV7hd/0POwKB\
     gQCl2vXwBLPamWCoZw61sJ+HKaq5yd7h1utqDbJcA9bhZ8CHUpbEBIa2frlUkMSv35jDorj4UjhG8NdQEcRzvAE1EJ+XlvEWsB+z\
     nHpUVA+JEUJ5QVD3D0BYCbs0dvzXWmUXzTi5eIkDRGLog+KQOziISIN1r1RsnzlQltfcRur4WwKBgCzd+AxFHD43XTu6FTU5vXtY\
     YdCM+C/Rt8xqItSYes7ZJAUZlo9EwO89i5M6/DzmuH0dDaA5U0pqyY1IX6QIBvvv5qu3gXhobJZ25rXmiOiA+Bt7cHwFG37XHNVf\
     5pjUmtYNcjYZ8Be6CU3yMPqEejCUlEB7XyS4EHA5JY2hCwHXAoGABdlJK+rm7PCS5mhuG1TLOJMW8baFYfVx21d6NKmCqbHlseIZ\
     e2inHl5Y28oe6C4b19ncbg37yfM3OzIA5wF1d0gBqbTnM6IeoV2Rup76BxGOITPsglEHOphyx/cTSGI6Gkqcrrwj+C09UQ+4hQgh\
     IcVnB3oJ43V9GVg+mWBMhrY=",
];

/// The test key `i`.
pub(crate) fn test_key(i: usize) -> RsaKeyPair {
    RsaKeyPair::from_pkcs8(&node_b64_decode(TEST_KEYS[i])).expect("a valid test key")
}

/// A compact JWT of `claims` (`alg` `RS256` signed with `key`, or `none` unsigned).
pub(crate) fn sign_jwt(claims: &Value, key: &RsaKeyPair, kid: &str, alg: &str) -> String {
    let header = json!({ "alg": alg, "kid": kid, "typ": "JWT" });
    let input =
        format!("{}.{}", b64_url(header.to_string().as_bytes()), b64_url(claims.to_string().as_bytes()));
    let signature = if alg == "RS256" {
        let mut sig = vec![0; key.public().modulus_len()];
        key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).expect("a signature");
        b64_url(&sig)
    } else {
        String::new()
    };
    format!("{input}.{signature}")
}

/// What the token endpoint signs: a test may change any of it ([`FakeOidc::tamper`]).
pub(crate) struct Signing {
    /// The ID token's claims.
    pub claims: Map<String, Value>,
    /// The test key that signs it.
    pub key: usize,
    /// The `kid` of its header.
    pub kid: String,
    /// The `alg` of its header (`RS256` or `none`).
    pub alg: &'static str,
}

/// Changes what the token endpoint signs.
pub(crate) type Tamper = Box<dyn Fn(&mut Signing) + Send>;

/// The query Google sends the browser back to the redirect URI with.
#[derive(Clone, Debug)]
pub(crate) struct Redirect {
    pub code: String,
    pub state: String,
    pub iss: Option<String>,
}

struct Code {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    claims: Map<String, Value>,
}

struct State {
    client_id: String,
    client_secret: String,
    clock: Arc<ManualClock>,
    key: usize,
    kid: String,
    jwks_fetches: usize,
    token_calls: Vec<BTreeMap<String, String>>,
    tamper: Option<Tamper>,
    codes: HashMap<String, Code>,
}

/// The fake provider, serving until dropped.
pub(crate) struct FakeOidc {
    state: Arc<Mutex<State>>,
    base: String,
    server: JoinHandle<()>,
}

impl Drop for FakeOidc {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl FakeOidc {
    /// A provider of the client `client_id` that signs with `clock`'s time.
    pub(crate) async fn start(client_id: &str, client_secret: &str, clock: Arc<ManualClock>) -> FakeOidc {
        let state = Arc::new(Mutex::new(State {
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            clock,
            key: 0,
            kid: "k1".into(),
            jwks_fetches: 0,
            token_calls: Vec::new(),
            tamper: None,
            codes: HashMap::new(),
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a local port");
        let base = format!("http://{}", listener.local_addr().expect("an address"));
        let shared = state.clone();
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let state = shared.clone();
                tokio::spawn(async move {
                    let service = service_fn(move |req| {
                        let state = state.clone();
                        async move { Ok::<_, Infallible>(answer(&state, req).await) }
                    });
                    let _ = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await;
                });
            }
        });
        FakeOidc { state, base, server }
    }

    /// The provider's endpoints: Google's consent page (never fetched), the local token and key
    /// endpoints.
    pub(crate) fn endpoints(&self) -> OidcEndpoints {
        OidcEndpoints {
            authorization: "https://accounts.google.com/o/oauth2/v2/auth".into(),
            token: format!("{}/token", self.base),
            jwks: format!("{}/certs", self.base),
            issuers: vec!["accounts.google.com".into(), ISSUER.into()],
        }
    }

    /// The options of an auth service using this provider (plain HTTP allowed).
    pub(crate) fn options(&self) -> OidcOptions {
        OidcOptions { endpoints: self.endpoints(), allow_http: true }
    }

    /// The user consents on Google's page opened at `auth_url` and picks the account `claims`:
    /// the query Google redirects with. Panics on an authorization request Google would refuse.
    pub(crate) fn authorize(&self, auth_url: &str, claims: Value) -> Redirect {
        let query: HashMap<String, String> =
            parse_urlencoded(auth_url.split_once('?').map_or("", |(_, q)| q)).into_iter().collect();
        let get = |k: &str| query.get(k).map(String::as_str).unwrap_or("");
        let mut st = self.state.lock();
        assert!(
            get("client_id") == st.client_id
                && check_redirect_uri(get("redirect_uri")).is_ok()
                && get("response_type") == "code"
                && get("code_challenge_method") == "S256"
                && !get("state").is_empty(),
            "a bad authorization request: {auth_url}"
        );
        let code = random_token("");
        let claims = claims.as_object().cloned().unwrap_or_default();
        st.codes.insert(
            code.clone(),
            Code {
                nonce: get("nonce").to_owned(),
                challenge: get("code_challenge").to_owned(),
                redirect_uri: get("redirect_uri").to_owned(),
                claims,
            },
        );
        Redirect { code, state: get("state").to_owned(), iss: Some(ISSUER.into()) }
    }

    /// Changes what the token endpoint signs (`None`: signs as Google would).
    pub(crate) fn tamper(&self, tamper: Option<Tamper>) {
        self.state.lock().tamper = tamper;
    }

    /// Signs with another key under the id `kid` from now on.
    pub(crate) fn rotate_key(&self, kid: &str) {
        let mut st = self.state.lock();
        st.key = 1;
        st.kid = kid.to_owned();
    }

    /// The fetches of the keys so far.
    pub(crate) fn jwks_fetches(&self) -> usize {
        self.state.lock().jwks_fetches
    }

    /// The forms posted to the token endpoint so far.
    pub(crate) fn token_calls(&self) -> Vec<BTreeMap<String, String>> {
        self.state.lock().token_calls.clone()
    }
}

fn json_answer(status: StatusCode, body: &Value, cache_control: Option<&str>) -> Response<Full<Bytes>> {
    let mut res = Response::builder().status(status).header("content-type", "application/json");
    if let Some(cc) = cache_control {
        res = res.header("cache-control", cc);
    }
    res.body(Full::new(Bytes::from(body.to_string()))).expect("a valid answer")
}

async fn answer(state: &Mutex<State>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    match (method, path.as_str()) {
        (Method::GET, "/certs") => {
            let mut st = state.lock();
            st.jwks_fetches += 1;
            let key = test_key(st.key);
            let public = PublicKeyComponents::<Vec<u8>>::from(key.public());
            let jwk = json!({
                "kty": "RSA",
                "n": b64_url(&public.n),
                "e": b64_url(&public.e),
                "kid": st.kid,
                "alg": "RS256",
                "use": "sig",
            });
            json_answer(
                StatusCode::OK,
                &json!({ "keys": [jwk] }),
                Some("public, max-age=3600, must-revalidate"),
            )
        }
        (Method::POST, "/token") => token(state, &String::from_utf8_lossy(&body)),
        _ => json_answer(StatusCode::NOT_FOUND, &json!({ "error": "not_found" }), None),
    }
}

fn token(state: &Mutex<State>, form: &str) -> Response<Full<Bytes>> {
    let f: BTreeMap<String, String> = parse_urlencoded(form).into_iter().collect();
    let get = |k: &str| f.get(k).map(String::as_str).unwrap_or("");
    let mut st = state.lock();
    st.token_calls.push(f.clone());
    let refuse = |status: StatusCode, error: &str| json_answer(status, &json!({ "error": error }), None);
    if get("client_id") != st.client_id || get("client_secret") != st.client_secret {
        return refuse(StatusCode::UNAUTHORIZED, "invalid_client");
    }
    if get("grant_type") != "authorization_code" {
        return refuse(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let Some(code) = st.codes.remove(get("code")) else {
        return refuse(StatusCode::BAD_REQUEST, "invalid_grant");
    };
    // As Google: the redirect URI of the code's authorization request (RFC 6749 section 4.1.3).
    if get("redirect_uri") != code.redirect_uri || pkce_challenge(get("code_verifier")) != code.challenge {
        return refuse(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let t = st.clock.wall_ms() / 1000;
    let mut claims = json!({
        "iss": ISSUER, "azp": st.client_id, "aud": st.client_id, "iat": t, "exp": t + 3600, "nonce": code.nonce,
    })
    .as_object()
    .cloned()
    .unwrap_or_default();
    claims.extend(code.claims);
    let mut signing = Signing { claims, key: st.key, kid: st.kid.clone(), alg: "RS256" };
    if let Some(tamper) = &st.tamper {
        tamper(&mut signing);
    }
    let id_token =
        sign_jwt(&Value::Object(signing.claims), &test_key(signing.key), &signing.kid, signing.alg);
    let body = json!({
        "access_token": "ya29.fake",
        "token_type": "Bearer",
        "expires_in": 3599,
        "scope": "openid email profile",
        "id_token": id_token,
    });
    json_answer(StatusCode::OK, &body, None)
}
