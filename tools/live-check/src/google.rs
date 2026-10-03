//! A fake Google (OpenID Connect) on 127.0.0.1 for the Google sign-in part: the consent step
//! ([`FakeGoogle::authorize`], called by the control route the game's browser opener uses), the
//! token endpoint (authorization code, PKCE S256, client secret, the redirect URI of the code's
//! request byte for byte) and the signing keys, with RS256 ID tokens signed by a fixed key made
//! for tests only.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use http::{Method, Request, Response, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use ring::rand::SystemRandom;
use ring::rsa::PublicKeyComponents;
use ring::signature::{RSA_PKCS1_SHA256, RsaKeyPair};
use scacelith_server::auth::{
    OidcEndpoints, OidcOptions, check_redirect_uri, form_urlencode, pkce_challenge,
};
use scacelith_server::http::url::parse_urlencoded;
use scacelith_server::security::encoding::{b64_url, node_b64_decode};
use scacelith_server::security::keys::random_token;
use serde_json::{Map, Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// The issuer of the ID tokens.
const ISSUER: &str = "https://accounts.google.com";

/// An RSA key (PKCS#8, base64) made for tests only: it signs the ID tokens.
const TEST_KEY: &str = "\
    MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQDDfrUy5G+HSAoEbjhXjcDbKj/dawXfiLpV2NbBJpDjKd7/Gdr/\
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
    L5wWKqGdsBTFS+i3LyTFWg==";

/// The `kid` of the signing key.
const KID: &str = "k1";

/// A code handed out at the consent step, waiting for its exchange.
struct Code {
    nonce: String,
    challenge: String,
    redirect_uri: String,
    claims: Map<String, Value>,
}

struct State {
    client_id: String,
    client_secret: String,
    key: RsaKeyPair,
    codes: HashMap<String, Code>,
}

/// The fake provider, serving until dropped.
pub struct FakeGoogle {
    state: Arc<Mutex<State>>,
    base: String,
    server: JoinHandle<()>,
}

impl std::fmt::Debug for FakeGoogle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FakeGoogle").field("base", &self.base).finish()
    }
}

impl Drop for FakeGoogle {
    fn drop(&mut self) {
        self.server.abort();
    }
}

/// The account picked on the consent page, or the user's refusal.
#[derive(Debug)]
pub enum Consent {
    /// The ID token's claims of the account picked (`sub`, `email`, `email_verified`, `name`).
    Account(Value),
    /// `error=<code>` (`access_denied`).
    Refused(String),
}

impl FakeGoogle {
    /// A provider of the OAuth client `client_id`.
    pub async fn start(client_id: &str, client_secret: &str) -> std::io::Result<FakeGoogle> {
        let key = RsaKeyPair::from_pkcs8(&node_b64_decode(TEST_KEY)).expect("the test key is valid");
        let state = Arc::new(Mutex::new(State {
            client_id: client_id.to_owned(),
            client_secret: client_secret.to_owned(),
            key,
            codes: HashMap::new(),
        }));
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}", listener.local_addr()?);
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
        Ok(FakeGoogle { state, base, server })
    }

    /// The options of a server signing in with this provider: Google's consent page (opened by
    /// the game's browser opener, which the control server answers instead), the local token and
    /// key endpoints, plain HTTP allowed.
    pub fn options(&self) -> OidcOptions {
        OidcOptions {
            endpoints: OidcEndpoints {
                authorization: "https://accounts.google.com/o/oauth2/v2/auth".into(),
                token: format!("{}/token", self.base),
                jwks: format!("{}/certs", self.base),
                issuers: vec!["accounts.google.com".into(), ISSUER.into()],
            },
            allow_http: true,
        }
    }

    /// Google's consent page opened at `auth_url`: the query of the redirect to the game's
    /// listener (`code`, `state`, `iss`, or `error`, `state`), or why Google would refuse the
    /// authorization request. The redirect URI is the request's.
    pub fn authorize(&self, auth_url: &str, consent: Consent) -> Result<(String, String), String> {
        let query: HashMap<String, String> =
            parse_urlencoded(auth_url.split_once('?').map_or("", |(_, q)| q)).into_iter().collect();
        let get = |k: &str| query.get(k).map_or("", String::as_str);
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        let valid = get("client_id") == st.client_id
            && check_redirect_uri(get("redirect_uri")).is_ok()
            && get("response_type") == "code"
            && get("code_challenge_method") == "S256"
            && !get("state").is_empty();
        if !valid {
            return Err(format!("a bad authorization request: {auth_url}"));
        }
        let answer = match consent {
            Consent::Refused(error) => form_urlencode(&[("error", &error), ("state", get("state"))]),
            Consent::Account(claims) => {
                let code = random_token("");
                st.codes.insert(
                    code.clone(),
                    Code {
                        nonce: get("nonce").to_owned(),
                        challenge: get("code_challenge").to_owned(),
                        redirect_uri: get("redirect_uri").to_owned(),
                        claims: claims.as_object().cloned().unwrap_or_default(),
                    },
                );
                form_urlencode(&[("code", &code), ("state", get("state")), ("iss", ISSUER)])
            }
        };
        Ok((get("redirect_uri").to_owned(), answer))
    }
}

fn json_answer(status: StatusCode, body: &Value, cache_control: Option<&str>) -> Response<Full<Bytes>> {
    let mut res = Response::builder().status(status).header("content-type", "application/json");
    if let Some(cc) = cache_control {
        res = res.header("cache-control", cc);
    }
    res.body(Full::new(Bytes::from(body.to_string())))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())))
}

async fn answer(state: &Mutex<State>, req: Request<Incoming>) -> Response<Full<Bytes>> {
    let (method, path) = (req.method().clone(), req.uri().path().to_owned());
    let body = req.into_body().collect().await.map(|b| b.to_bytes()).unwrap_or_default();
    match (method, path.as_str()) {
        (Method::GET, "/certs") => {
            let st = state.lock().unwrap_or_else(|p| p.into_inner());
            let public = PublicKeyComponents::<Vec<u8>>::from(st.key.public());
            let jwk = json!({
                "kty": "RSA", "n": b64_url(&public.n), "e": b64_url(&public.e), "kid": KID, "alg": "RS256", "use": "sig",
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

/// The code exchange, as Google: the client's credentials, the code once, its redirect URI and
/// the PKCE verifier; an ID token signed with the test key.
fn token(state: &Mutex<State>, form: &str) -> Response<Full<Bytes>> {
    let f: HashMap<String, String> = parse_urlencoded(form).into_iter().collect();
    let get = |k: &str| f.get(k).map_or("", String::as_str);
    let mut st = state.lock().unwrap_or_else(|p| p.into_inner());
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
    if get("redirect_uri") != code.redirect_uri || pkce_challenge(get("code_verifier")) != code.challenge {
        return refuse(StatusCode::BAD_REQUEST, "invalid_grant");
    }
    let t = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let mut claims = Map::new();
    for (k, v) in [
        ("iss", json!(ISSUER)),
        ("azp", json!(st.client_id)),
        ("aud", json!(st.client_id)),
        ("iat", json!(t)),
        ("exp", json!(t + 3600)),
        ("nonce", json!(code.nonce)),
    ] {
        claims.insert(k.to_owned(), v);
    }
    claims.extend(code.claims);
    let header = json!({ "alg": "RS256", "kid": KID, "typ": "JWT" });
    let input = format!(
        "{}.{}",
        b64_url(header.to_string().as_bytes()),
        b64_url(Value::Object(claims).to_string().as_bytes())
    );
    let mut sig = vec![0; st.key.public().modulus_len()];
    if st.key.sign(&RSA_PKCS1_SHA256, &SystemRandom::new(), input.as_bytes(), &mut sig).is_err() {
        return refuse(StatusCode::INTERNAL_SERVER_ERROR, "signature");
    }
    let body = json!({
        "access_token": "ya29.fake",
        "token_type": "Bearer",
        "expires_in": 3599,
        "scope": "openid email profile",
        "id_token": format!("{input}.{}", b64_url(&sig)),
    });
    json_answer(StatusCode::OK, &body, None)
}
