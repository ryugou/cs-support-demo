use super::{AuthError, VerifiedIdentity};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";
/// 検証済み identity のキャッシュエントリに許す TTL 上限。Google tokeninfo への RTT を抑えるための
/// 短時間キャッシュであり、実トークンの `expires_in` がこれより長くても、失効直後の判定が
/// 古くなり過ぎないようここで頭打ちにする（許容トレードオフ。トークン単位）。
const MAX_CACHE_TTL: Duration = Duration::from_secs(300);

/// Cloud Run の request deadline（既定値 300 秒）。tokeninfo の timeout をこれより十分短く
/// 取るための基準としてのみ使う（定数の根拠を近くに残すための宣言。テストで大小関係を検証する）。
const CLOUD_RUN_REQUEST_DEADLINE: Duration = Duration::from_secs(300);

/// tokeninfo への接続確立 timeout。
/// 全リクエストがこの検証を通るため、timeout 未設定（旧 `reqwest::Client::new()`）だと
/// Google 側の遅延・ネットワーク障害でタスクが滞留し、fail-closed ではなく実質停止する。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// tokeninfo リクエスト全体（接続 + 送信 + 応答本文読み切り）の timeout。
///
/// 値の根拠:
/// - tokeninfo は Google への小さな POST 1 本で、観測レイテンシは 1 秒未満。
///   5 秒は正常系の十分な余裕（>5x）を持つ。
/// - Cloud Run の request deadline（`CLOUD_RUN_REQUEST_DEADLINE` = 300 秒）に対して
///   5 秒は 2% 未満。認証で deadline を使い切らず、残りを本来の MCP 処理に残せる。
/// - Google 障害時はここで打ち切って `AuthError::Unreachable` → 503 に倒す。
///   ぶら下がったまま worker task を占有し続けるより、速く落ちて運用者に見える方がよい。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

// 上記の根拠（「deadline に対して十分短い」）をコメントだけに委ねず、コンパイル時に強制する。
// 将来 timeout を伸ばす変更が入っても、deadline の 1/10 を超えた時点でビルドが落ちる。
const _: () = assert!(
    REQUEST_TIMEOUT.as_secs() * 10 <= CLOUD_RUN_REQUEST_DEADLINE.as_secs(),
    "tokeninfo request timeout must stay far below the Cloud Run request deadline"
);

/// Google tokeninfo のレスポンス（フィールドは Google 仕様通りすべて文字列で返る）。
#[derive(Debug, Deserialize)]
pub(crate) struct TokenInfo {
    pub aud: Option<String>,
    pub azp: Option<String>,
    /// Google の安定した principal ID。email と違い変更・再割当てされないため、
    /// actor ID と WORM 監査の主識別子に使う（F4）。
    pub sub: Option<String>,
    pub email: Option<String>,
    pub email_verified: Option<String>,
    /// トークンの残り有効秒数（Google 仕様通り文字列）。無い/パース不可な応答もあり得る。
    /// その場合は「キャッシュしない」に倒す（`cache_ttl` 参照）。
    pub expires_in: Option<String>,
}

/// キャッシュ TTL を決める純粋関数（ネットワーク非依存・単体テスト対象）。
/// 実トークンの残り秒数 `expires_in` と `MAX_CACHE_TTL` の小さい方を採用する。
///
/// `None` を返した場合は **キャッシュしない**（毎回 tokeninfo に再照会する）。
///
/// 2026-07 改訂 / codex adversarial-review F2:
/// 旧実装は `expires_in` が欠落・パース不可のとき無条件に `MAX_CACHE_TTL`（300 秒）を
/// 採用していた。これは「不明な入力に最大限の信頼を与える」向きで fail-safe が逆である
/// （形式不明な応答ほど、失効済みトークンを 5 分間受理し続ける危険が大きい）。
/// 現行は残り有効秒数を確定できない応答をキャッシュ対象から外す。コストは tokeninfo への
/// 再照会 1 回であり、認証の正しさより安い。
pub(crate) fn cache_ttl(expires_in: Option<&str>) -> Option<Duration> {
    let secs = expires_in?.parse::<u64>().ok()?;
    if secs == 0 {
        // 残り 0 秒 = 実質失効済み。キャッシュすると失効トークンを延命してしまう。
        return None;
    }
    Some(Duration::from_secs(secs).min(MAX_CACHE_TTL))
}

/// tokeninfo 応答から検証済み identity（安定した `sub` + 当時の email）を導く純粋関数
/// （ネットワーク非依存・単体テスト対象）。
///
/// audience 判定（2026-07 改訂 / codex adversarial-review F1）:
/// 旧実装は `aud == client_id || azp == client_id` の **OR** だった。これはリソースサーバの
/// audience 厳密性を失う。別 audience 向けに発行されたトークンでも、azp が自クライアントを
/// 指してさえいれば受理されてしまうためである。
///
/// 現行は `aud` の完全一致を **必須** とし、`azp` は「aud 一致を救済しない追加条件」として
/// 扱う（存在する場合は同じく完全一致を要求。異なる client を指す azp はトークンの
/// 委譲・なりすまし経路を示すため拒否する）。
///
/// 実 Google tokeninfo（`POST https://oauth2.googleapis.com/tokeninfo`、access_token は
/// フォームボディで送る。クエリに載せるとエラー文字列やアクセスログから漏れるため）が
/// authorization-code フローで発行されたユーザアクセストークンに対して返す形（2026-07 観測）:
/// ```text
/// {"azp":"<client_id>","aud":"<client_id>","sub":"<21桁の数値>",
///  "scope":"...","exp":"...","expires_in":"3599",
///  "email":"...","email_verified":"true","access_type":"offline"}
/// ```
/// すなわち正常系では `aud == azp == client_id` であり、`aud` 必須化で正常系は落ちない。
pub(crate) fn decide(
    info: &TokenInfo,
    expected_client_id: &str,
) -> Result<VerifiedIdentity, AuthError> {
    // aud 完全一致は必須。欠落は「一致」に読み替えない（不明な入力を信頼しない）。
    if info.aud.as_deref() != Some(expected_client_id) {
        // 「aud 不一致だが azp は一致」は、旧 OR 実装なら通っていた唯一の組み合わせ。
        // 厳密化が本番の正常系を壊した場合はここだけが鳴るため、想定外の回帰を
        // 一目で切り分けられるよう、通常の Invalid とは別に warn で明示する。
        // client_id は秘密ではない（公開クライアント識別子）ためログ可。token は載せない。
        if info.azp.as_deref() == Some(expected_client_id) {
            tracing::warn!(
                reason = "aud_mismatch_with_matching_azp",
                aud = ?info.aud,
                expected_client_id = %expected_client_id,
                "rejected a token that the previous aud||azp rule would have accepted"
            );
        }
        return Err(AuthError::Invalid(
            "token audience (aud) does not match this client".into(),
        ));
    }
    // azp は追加条件。存在しない応答もあり得るため、存在する場合のみ一致を要求する。
    if info.azp.is_some() && info.azp.as_deref() != Some(expected_client_id) {
        // aud 一致・azp 不一致でここに落ちるのは、旧 OR 実装なら通っていた組み合わせであり、
        // かつ `:100` の aud 判定を素通りしているため aud_mismatch_with_matching_azp は鳴らない。
        // 専用 reason を出さないと、本番ログインが壊れたときに「aud で落ちたのか azp で
        // 落ちたのか」をログだけで切り分けられない。両条件に同じ観測性を与える。
        tracing::warn!(
            reason = "azp_mismatch",
            azp = ?info.azp,
            expected_client_id = %expected_client_id,
            "rejected a token whose authorized party differs from this client"
        );
        return Err(AuthError::Invalid(
            "token authorized party (azp) does not match this client".into(),
        ));
    }
    if info.email_verified.as_deref() != Some("true") {
        return Err(AuthError::Invalid("email is not verified".into()));
    }
    let email = info
        .email
        .clone()
        .ok_or_else(|| AuthError::Invalid("token has no email claim".into()))?;
    // `sub` 欠落は fail closed（F4）。実 tokeninfo はユーザアクセストークンに対し必ず
    // 返すため、欠落は想定外の応答である。ここで email へ暗黙にフォールバックすると
    // 監査主体の識別子体系が静かに二重化するため、認証ごと拒否する。
    let sub = info
        .sub
        .clone()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| AuthError::Invalid("token has no stable subject (sub) claim".into()))?;
    Ok(VerifiedIdentity { sub, email })
}

/// URL から秘密が載りうる部分（クエリ・フラグメント・userinfo）を落として文字列化する。
/// 運用者に「どこへ繋ごうとして失敗したか」を見せつつ、`?access_token=…` のような
/// 秘密をログへ持ち出さないための唯一の経路。
fn sanitize_url(url: &reqwest::Url) -> String {
    let mut safe = url.clone();
    safe.set_query(None);
    safe.set_fragment(None);
    // set_username / set_password は cannot-be-a-base URL で Err を返すが、
    // その場合は元から userinfo を持たないため無視してよい。
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.to_string()
}

/// reqwest の `Error` を、秘密を含まない固定文言へ分類して落とす。
///
/// reqwest 0.12 の `Display for Error` は末尾に必ずリクエスト URL を書き出す
/// （`src/error.rs`: `write!(f, " for url ({url})")`）。timeout / body 経路も
/// `.with_url(...)` で URL を保持する。tokeninfo をクエリ付きで叩いていた旧実装では、
/// この Display をそのままログに流すことで、有効期限内の生アクセストークンが
/// Cloud Logging（トークン本体より遥かに広い閲覧母集団と長い保持期間を持つ）へ
/// 残っていた。ここで `Display` を絶対に埋め込まないことが、POST 化と並ぶ
/// 二層目の防御になる。
fn describe_transport_error(stage: &str, err: &reqwest::Error) -> String {
    let kind = if err.is_timeout() {
        "timed out"
    } else if err.is_connect() {
        "connection failed"
    } else if err.is_body() {
        "response body could not be read"
    } else if err.is_decode() {
        "response could not be decoded"
    } else if err.is_redirect() {
        "too many redirects"
    } else if err.is_request() {
        "request could not be sent"
    } else if err.is_builder() {
        // `.form()` のシリアライズ失敗などはここに来る。Google 側の障害ではなく
        // こちらの設定・実装の不備なので、到達不能と同じ文言に潰さない。
        "request could not be built"
    } else if err.is_status() {
        // 現行の呼び出しは `error_for_status()` を通していないため通常は発生しないが、
        // 将来 status 由来の Error が混ざったときに「不明」へ落ちないようにしておく。
        "endpoint returned an error status"
    } else if err.is_upgrade() {
        "protocol upgrade failed"
    } else {
        // reqwest 0.12 の Kind 全 7 種（Builder / Request / Redirect / Status / Body /
        // Decode / Upgrade）と timeout / connect を上で網羅済み。ここへ来るのは
        // reqwest 側に新しい分類が増えたときだけで、その検知が本 else の役割。
        "unknown transport failure"
    };
    let endpoint = err.url().map(sanitize_url);
    match endpoint {
        Some(url) => format!("{stage}: {kind} (endpoint: {url})"),
        None => format!("{stage}: {kind}"),
    }
}

/// Bearer トークンを Google tokeninfo endpoint に照会して検証する。
/// 到達不能・非2xx（400/401除く）・parse失敗は `AuthError::Unreachable` として区別する
/// （運用者が「Google 側の障害/ネットワーク問題」と「トークン自体が無効」を切り分けられるようにするため）。
pub struct GoogleTokenVerifier {
    client_id: String,
    http: reqwest::Client,
    tokeninfo_url: String,
    cache: Mutex<HashMap<String, (VerifiedIdentity, Instant)>>,
}

impl GoogleTokenVerifier {
    pub fn new(client_id: String) -> Self {
        Self::with_settings(
            client_id,
            TOKENINFO_URL.to_string(),
            CONNECT_TIMEOUT,
            REQUEST_TIMEOUT,
        )
    }

    /// endpoint と timeout を明示して構築する（本番は `new`、テストは stub サーバを指す）。
    fn with_settings(
        client_id: String,
        tokeninfo_url: String,
        connect_timeout: Duration,
        request_timeout: Duration,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(connect_timeout)
            .timeout(request_timeout)
            .build()
            // build が失敗するのは TLS backend の初期化不能などプロセス起動レベルの
            // 環境不備のみ。ここで `Client::new()` にフォールバックすると timeout 無しの
            // client に静かに戻ってしまい、本 fix が無効化される。起動時に大きな音で
            // 落ちる方が、無制限 timeout のまま本番稼働するより安全（fail closed）。
            .expect("failed to build reqwest client with timeouts for Google tokeninfo");
        Self {
            client_id,
            http,
            tokeninfo_url,
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Bearer を Google tokeninfo で検証し、安定した principal ID（`sub`）と email を返す。
    /// トークン単位で最大 5 分キャッシュする（`expires_in` を確定できない応答はキャッシュしない）。
    pub async fn verify(&self, bearer_token: &str) -> Result<VerifiedIdentity, AuthError> {
        if let Some(identity) = self.cached(bearer_token) {
            return Ok(identity);
        }
        // access_token は URL クエリではなく POST のフォームボディで送る。
        // クエリに載せると、reqwest のエラー文字列・中間プロキシのアクセスログ・
        // リトライログなど複数経路から生トークンが染み出す。Google tokeninfo は
        // POST + `application/x-www-form-urlencoded` を受け付けるため、送信形式ごと変える。
        let resp = self
            .http
            .post(&self.tokeninfo_url)
            .form(&[("access_token", bearer_token)])
            .send()
            .await
            .map_err(|e| {
                AuthError::Unreachable(describe_transport_error("tokeninfo request", &e))
            })?;
        if resp.status() == reqwest::StatusCode::BAD_REQUEST
            || resp.status() == reqwest::StatusCode::UNAUTHORIZED
        {
            return Err(AuthError::Invalid(
                "token rejected by Google tokeninfo".into(),
            ));
        }
        if !resp.status().is_success() {
            return Err(AuthError::Unreachable(format!(
                "tokeninfo returned {}",
                resp.status()
            )));
        }
        // reqwest の `json` feature は有効化していない（依存を増やさない方針）ため、
        // llm.rs と同様 text() + serde_json::from_str で手動 parse する。
        let text = resp.text().await.map_err(|e| {
            AuthError::Unreachable(describe_transport_error("tokeninfo body read", &e))
        })?;
        let info: TokenInfo = serde_json::from_str(&text)
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo parse failed: {e}")))?;
        let identity = decide(&info, &self.client_id)?;
        match cache_ttl(info.expires_in.as_deref()) {
            Some(ttl) => self.store(bearer_token, &identity, ttl),
            // 残り有効秒数を確定できない応答はキャッシュしない（F2）。無音で素通りすると
            // 「なぜか tokeninfo QPS だけ上がった」の原因が追えないため、応答値ごと記録する
            // （expires_in は秒数であり秘密情報ではない。token 本体は絶対にログしない）。
            None => tracing::warn!(
                expires_in = ?info.expires_in,
                "tokeninfo response has no usable expires_in; not caching this verification"
            ),
        }
        Ok(identity)
    }

    fn cached(&self, token: &str) -> Option<VerifiedIdentity> {
        // lock 保持は HashMap 参照のみの短時間。await をまたがないので poison 化のリスクは実質ない。
        let cache = self.cache.lock().unwrap();
        cache
            .get(token)
            .filter(|(_, exp)| *exp > Instant::now())
            .map(|(identity, _)| identity.clone())
    }

    fn store(&self, token: &str, identity: &VerifiedIdentity, ttl: Duration) {
        let mut cache = self.cache.lock().unwrap();
        // insert 前に期限切れエントリを purge する。TTL は実トークンの exp 由来（`cache_ttl`）
        // なので、これで「有効なトークン数」に比例した大きさに有界化される
        // （TTL 無視の固定 300 秒だと、失効済みトークンのエントリが最大 300 秒分溜まり続けていた）。
        let now = Instant::now();
        cache.retain(|_, (_, exp)| *exp > now);
        cache.insert(token.to_string(), (identity.clone(), now + ttl));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    /// 応答を返さずに接続を保持し続けるテスト用サーバ（Google 側の遅延・ハング相当）。
    /// listener と接続を `JoinHandle` 内に保持し続けることで、TCP は繋がるが
    /// レスポンスが永遠に来ない状態を作る。
    async fn spawn_hanging_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                // 読みも書きもせず保持するだけ。drop すると接続が切れて
                // timeout ではなく即時エラーになってしまうため、明示的に持ち続ける。
                held.push(stream);
            }
        });
        format!("http://{addr}/tokeninfo")
    }

    /// 固定 JSON を返すテスト用 tokeninfo サーバ。受けたリクエスト数を数え、
    /// 受信した生リクエスト（リクエスト行 + ヘッダ + ボディ）も記録する。
    /// `Connection: close` で応答するため keep-alive による接続再利用が起きず、
    /// 「tokeninfo を何回叩いたか」が接続数としてそのまま観測できる。
    async fn spawn_tokeninfo_stub(body: &'static str) -> (String, Arc<AtomicUsize>, RequestLog) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_for_task = hits.clone();
        let requests: RequestLog = Arc::new(Mutex::new(Vec::new()));
        let requests_for_task = requests.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                hits_for_task.fetch_add(1, Ordering::SeqCst);
                let requests_for_conn = requests_for_task.clone();
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let raw = read_full_request(&mut stream).await;
                    requests_for_conn.lock().unwrap().push(raw);
                    let resp = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                         Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        (format!("http://{addr}/tokeninfo"), hits, requests)
    }

    /// stub が受け取った生リクエストの記録。POST 移行後は access_token が
    /// URL ではなくボディに載ることを、ここに溜めた文字列で直接検証する。
    type RequestLog = Arc<Mutex<Vec<String>>>;

    /// ヘッダ終端まで読み、`Content-Length` があればその分のボディも読み切る。
    /// 1 回の `read` で足りる前提を置くと、ヘッダとボディが別セグメントで届いた際に
    /// ボディ検証が偶発的に失敗するため、明示的に読み切る。
    async fn read_full_request(stream: &mut tokio::net::TcpStream) -> String {
        use tokio::io::AsyncReadExt;
        let mut raw = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            raw.extend_from_slice(&buf[..n]);
            let text = String::from_utf8_lossy(&raw).to_string();
            let Some(header_end) = text.find("\r\n\r\n") else {
                continue;
            };
            let body_len = text[..header_end]
                .lines()
                .find_map(|l| {
                    let (name, value) = l.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            if raw.len() >= header_end + 4 + body_len {
                break;
            }
        }
        String::from_utf8_lossy(&raw).to_string()
    }

    /// ヘッダだけ返し、宣言した `Content-Length` に満たないボディで接続を切るサーバ。
    /// reqwest のボディ読み取り（`resp.text()`）を失敗させ、`Kind::Body` の
    /// エラー経路（URL を保持する）を踏ませるために使う。
    async fn spawn_truncated_body_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::AsyncWriteExt;
                    let _ = read_full_request(&mut stream).await;
                    // Content-Length は 4096 と宣言するが、実際には数バイトしか
                    // 書かずに切断する → クライアント側は本文読み切りに失敗する。
                    let _ = stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                              Content-Length: 4096\r\n\r\n{\"aud\":",
                        )
                        .await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}/tokeninfo")
    }

    /// テストで使う、実トークンを模した一意な文字列。
    /// エラー文字列に「部分文字列として現れないこと」を assert する対象。
    const SECRET_TOKEN: &str = "ya29.SECRET-ACCESS-TOKEN-do-not-log-9f3a1c";

    /// `AuthError` の Display / Debug 双方の表現を集めて返す。
    /// 片方だけ検査すると、もう片方の経路（`{:?}` でのログ出力など）から漏れる。
    fn rendered(err: &AuthError) -> String {
        let inner = match err {
            AuthError::Invalid(m) | AuthError::Unreachable(m) => m.clone(),
            AuthError::Missing => String::new(),
        };
        format!("{err:?}|{inner}")
    }

    /// C1: tokeninfo が応答しない（timeout）とき、生アクセストークンがエラー文字列に
    /// 一切現れない。reqwest の `Error` は Display/Debug の末尾に必ずリクエスト URL を
    /// 書き出すため、旧実装（`format!("{e}")`）はクエリ文字列ごとトークンを露出していた。
    #[tokio::test]
    async fn timeout_error_never_contains_the_access_token() {
        let url = spawn_hanging_server().await;
        let verifier = GoogleTokenVerifier::with_settings(
            "client-123".to_string(),
            url,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let err = verifier.verify(SECRET_TOKEN).await.unwrap_err();
        let text = rendered(&err);
        assert!(
            !text.contains(SECRET_TOKEN),
            "access token leaked into AuthError on timeout path: {text}"
        );
    }

    /// C1: 本文読み取り失敗（`Kind::Body`）の経路でも生アクセストークンが漏れない。
    /// この経路の reqwest エラーも URL を保持するため、timeout とは別に検証する。
    #[tokio::test]
    async fn body_read_error_never_contains_the_access_token() {
        let url = spawn_truncated_body_server().await;
        let verifier = stub_verifier(url);
        let err = verifier.verify(SECRET_TOKEN).await.unwrap_err();
        let text = rendered(&err);
        assert!(
            matches!(err, AuthError::Unreachable(_)),
            "truncated body must be classified as Unreachable, got: {err:?}"
        );
        // このテストが本当に body 読み取り経路（reqwest `Kind::Body`）を踏んでいることを
        // 固定する。踏めていないと「漏れない」の assert が空振りしても気づけない。
        assert!(
            text.contains("tokeninfo body read"),
            "expected the body-read error path, got: {text}"
        );
        assert!(
            !text.contains(SECRET_TOKEN),
            "access token leaked into AuthError on body-read path: {text}"
        );
    }

    /// C1（根本修正）: access_token は URL クエリではなく POST のフォームボディで送る。
    /// クエリに載せる限り、reqwest のエラー・中間プロキシのアクセスログ・再試行ログなど
    /// 複数経路から生トークンが染み出す。送信形式そのものを変えて経路ごと塞ぐ。
    #[tokio::test]
    async fn tokeninfo_is_called_with_post_and_token_in_body_not_url() {
        let (url, _hits, requests) = spawn_tokeninfo_stub(STUB_WITH_EXPIRES).await;
        let verifier = stub_verifier(url);
        verifier.verify(SECRET_TOKEN).await.expect("verify");
        let captured = requests.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "expected exactly one tokeninfo request");
        let raw = &captured[0];
        let request_line = raw.lines().next().unwrap_or_default();
        assert!(
            request_line.starts_with("POST /tokeninfo"),
            "tokeninfo must be called with POST, got request line: {request_line}"
        );
        assert!(
            !request_line.contains(SECRET_TOKEN),
            "access token must not appear in the request line (URL): {request_line}"
        );
        assert!(
            raw.contains(&format!("access_token={SECRET_TOKEN}")),
            "access token must be sent in the form body"
        );
    }

    /// C1: URL を運用者に見せる場合でも、クエリ文字列は落としてから見せる。
    #[test]
    fn sanitized_url_drops_query_and_credentials() {
        let url = reqwest::Url::parse(
            "https://user:pw@oauth2.googleapis.com/tokeninfo?access_token=abc#f",
        )
        .unwrap();
        let got = sanitize_url(&url);
        assert!(
            !got.contains("access_token"),
            "query must be dropped: {got}"
        );
        assert!(!got.contains("abc"), "query value must be dropped: {got}");
        assert!(!got.contains("pw"), "credentials must be dropped: {got}");
        assert!(
            got.contains("oauth2.googleapis.com/tokeninfo"),
            "got: {got}"
        );
    }

    /// F3: tokeninfo が応答しないとき、timeout で打ち切られ `Unreachable` に分類される。
    /// `Invalid` に落ちると「トークンが無効」と誤診され、運用者が Google 側の障害を
    /// クライアント側のトークン不備と取り違えるため、この区別が重要。
    #[tokio::test]
    async fn hanging_tokeninfo_times_out_as_unreachable() {
        let url = spawn_hanging_server().await;
        let verifier = GoogleTokenVerifier::with_settings(
            "client-123".to_string(),
            url,
            Duration::from_millis(200),
            Duration::from_millis(200),
        );
        let started = Instant::now();
        let err = verifier.verify("some-token").await.unwrap_err();
        assert!(
            matches!(err, AuthError::Unreachable(_)),
            "timeout must be classified as Unreachable, got: {err:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "verify must be bounded by the configured timeout"
        );
    }

    /// F3: 実運用の既定値が「無制限」でないことを型ではなく設定として保証する。
    /// `new` は必ず timeout 付き client を組み立てる（組み立てに失敗すれば panic する）。
    #[test]
    fn default_constructor_builds_client_with_timeouts() {
        let v = GoogleTokenVerifier::new("client-123".to_string());
        assert_eq!(v.tokeninfo_url, TOKENINFO_URL);
        assert!(REQUEST_TIMEOUT < CLOUD_RUN_REQUEST_DEADLINE);
        assert!(CONNECT_TIMEOUT <= REQUEST_TIMEOUT);
    }

    /// 実 Google tokeninfo 応答（2026-07 観測）と同じキー構成の正常系 stub。
    const STUB_WITH_EXPIRES: &str = r#"{"azp":"client-123","aud":"client-123","sub":"101572111487015263315","email":"a@sivira.co","email_verified":"true","expires_in":"3599"}"#;
    /// `expires_in` を欠いた応答（形式ゆれ・仕様変更相当）。
    const STUB_WITHOUT_EXPIRES: &str = r#"{"azp":"client-123","aud":"client-123","sub":"101572111487015263315","email":"a@sivira.co","email_verified":"true"}"#;

    fn stub_verifier(url: String) -> GoogleTokenVerifier {
        GoogleTokenVerifier::with_settings(
            "client-123".to_string(),
            url,
            Duration::from_secs(2),
            Duration::from_secs(2),
        )
    }

    /// `expires_in` があるときは 2 回目の検証がキャッシュから返り、tokeninfo を再度叩かない。
    #[tokio::test]
    async fn verification_is_cached_when_expires_in_is_present() {
        let (url, hits, _requests) = spawn_tokeninfo_stub(STUB_WITH_EXPIRES).await;
        let verifier = stub_verifier(url);
        verifier.verify("token-a").await.expect("first verify");
        verifier.verify("token-a").await.expect("second verify");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            1,
            "second verify must be served from cache"
        );
    }

    /// F2: `expires_in` が無い応答はキャッシュせず、毎回 tokeninfo に再照会する。
    /// 検証自体は成功する（TTL を決められないだけで、応答内容は妥当なため）。
    #[tokio::test]
    async fn verification_is_not_cached_when_expires_in_is_missing() {
        let (url, hits, _requests) = spawn_tokeninfo_stub(STUB_WITHOUT_EXPIRES).await;
        let verifier = stub_verifier(url);
        verifier.verify("token-b").await.expect("first verify");
        verifier.verify("token-b").await.expect("second verify");
        assert_eq!(
            hits.load(Ordering::SeqCst),
            2,
            "response without expires_in must not be cached"
        );
    }

    fn info(aud: &str, verified: &str, email: Option<&str>) -> TokenInfo {
        TokenInfo {
            aud: Some(aud.to_string()),
            azp: None,
            sub: Some("101572111487015263315".to_string()),
            email: email.map(ToString::to_string),
            email_verified: Some(verified.to_string()),
            expires_in: None,
        }
    }

    #[test]
    fn accepts_matching_aud_and_verified_email() {
        let got = decide(
            &info("client-123", "true", Some("a@sivira.co")),
            "client-123",
        )
        .unwrap();
        assert_eq!(got.email, "a@sivira.co");
    }

    /// F4: 安定した principal ID（tokeninfo `sub`）を検証結果に含める。
    /// email は変更・別名・再割当てがありうるため監査主体として不安定であり、
    /// 主識別子には `sub` を使う。
    #[test]
    fn returns_stable_google_subject_alongside_email() {
        let got = decide(
            &info("client-123", "true", Some("a@sivira.co")),
            "client-123",
        )
        .unwrap();
        assert_eq!(got.sub, "101572111487015263315");
        assert_eq!(got.email, "a@sivira.co");
    }

    /// `sub` が無い応答は拒否する（fail closed）。
    /// 実 tokeninfo はユーザアクセストークンに対し必ず `sub` を返す（2026-07 観測）。
    /// 欠落しているのは想定外の応答であり、安定した監査主体を作れない以上、
    /// email へ暗黙にフォールバックせず拒否する（監査主体の一貫性を壊さない）。
    #[test]
    fn rejects_response_without_subject() {
        let mut i = info("client-123", "true", Some("a@sivira.co"));
        i.sub = None;
        let err = decide(&i, "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    /// 空文字の `sub` も欠落と同様に拒否する。
    #[test]
    fn rejects_response_with_empty_subject() {
        let mut i = info("client-123", "true", Some("a@sivira.co"));
        i.sub = Some(String::new());
        let err = decide(&i, "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn rejects_aud_mismatch() {
        let err = decide(&info("other", "true", Some("a@sivira.co")), "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn rejects_unverified_email() {
        let err = decide(
            &info("client-123", "false", Some("a@sivira.co")),
            "client-123",
        )
        .unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    #[test]
    fn rejects_missing_email() {
        let err = decide(&info("client-123", "true", None), "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    /// F1: リソースサーバとしての audience 厳密性。
    /// `azp`（authorized party）が一致していても、`aud` が別 audience なら受理しない。
    /// 旧実装は `aud || azp` の OR だったため、他 audience 向けに発行されたトークンが
    /// azp 一致だけで通っていた。
    #[test]
    fn rejects_aud_mismatch_even_when_azp_matches() {
        let mut i = info("aud-other", "true", Some("a@sivira.co"));
        i.azp = Some("client-123".to_string());
        let err = decide(&i, "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    /// `aud` そのものが欠落している応答も拒否する（欠落を「一致」に読み替えない）。
    #[test]
    fn rejects_missing_aud_even_when_azp_matches() {
        let mut i = info("client-123", "true", Some("a@sivira.co"));
        i.aud = None;
        i.azp = Some("client-123".to_string());
        let err = decide(&i, "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    /// `aud` 一致は必須だが、`azp` が存在して別クライアントを指す場合も拒否する
    /// （`aud` 一致で救済しない追加条件）。
    #[test]
    fn rejects_azp_mismatch_even_when_aud_matches() {
        let mut i = info("client-123", "true", Some("a@sivira.co"));
        i.azp = Some("other-client".to_string());
        let err = decide(&i, "client-123").unwrap_err();
        assert!(matches!(err, AuthError::Invalid(_)));
    }

    /// 実 Google tokeninfo の正常応答は `aud == azp == client_id`（観測値、下記 `decide` の
    /// doc コメント参照）。この組み合わせは受理する。
    #[test]
    fn accepts_when_both_aud_and_azp_match() {
        let mut i = info("client-123", "true", Some("a@sivira.co"));
        i.azp = Some("client-123".to_string());
        assert_eq!(decide(&i, "client-123").unwrap().email, "a@sivira.co");
    }

    #[test]
    fn cache_ttl_uses_expires_in_when_shorter_than_cap() {
        assert_eq!(cache_ttl(Some("120")), Some(Duration::from_secs(120)));
    }

    #[test]
    fn cache_ttl_caps_expires_in_at_max_cache_ttl() {
        assert_eq!(cache_ttl(Some("9999")), Some(MAX_CACHE_TTL));
    }

    /// F2: `expires_in` 欠落時は「最大限信頼する」のではなくキャッシュしない。
    /// 不明な入力に MAX_CACHE_TTL を与えるのは fail-safe の向きが逆だった。
    #[test]
    fn cache_ttl_is_none_when_expires_in_missing() {
        assert_eq!(cache_ttl(None), None);
    }

    #[test]
    fn cache_ttl_is_none_when_expires_in_unparseable() {
        assert_eq!(cache_ttl(Some("not-a-number")), None);
    }

    /// 負値・空文字も u64 としてパース不可 → キャッシュしない。
    #[test]
    fn cache_ttl_is_none_for_negative_or_empty_expires_in() {
        assert_eq!(cache_ttl(Some("-1")), None);
        assert_eq!(cache_ttl(Some("")), None);
    }

    /// 既に失効している（残り 0 秒）トークンはキャッシュしない。
    #[test]
    fn cache_ttl_is_none_when_expires_in_is_zero() {
        assert_eq!(cache_ttl(Some("0")), None);
    }
}
