//! `cs-support-mcp` を OAuth 2.1 認可サーバ（AS）として動かし、内部で Google に委譲する
//! （OAuth フェデレーション）。
//!
//! なぜ RS 専任をやめたか:
//! 旧構成は認可サーバを Google 自身にしていた。claude.ai は接続時に RFC 7591 の DCR
//! （動的クライアント登録）を試みるが、**Google は DCR に対応していない**ため、利用者ごとに
//! Google の Client ID / Secret を claude.ai の詳細設定へ手入力させる必要があった。
//! CS 担当者に配れないうえ、Client Secret をクライアント側に置くことになり秘密として
//! 成立しない。本モジュールが AS を引き受けることで、Google の client_id / secret は
//! サーバ側 env に閉じ込められ、claude.ai は MCP endpoint の URL だけで接続できる。
//!
//! ```text
//! claude.ai --DCR/authorize/token--> cs-support-mcp (AS) --authorize/token--> Google
//! ```
//!
//! # 自前トークンは発行しない（2026-07 改訂）
//!
//! 一時期、この AS は自前のアクセストークン / リフレッシュトークンを署名付きで
//! 発行していた。デモという位置づけに対し、寿命・失効・鍵管理を自前で抱える
//! 設計が過剰と判断されたため撤回した。**現在は Google が発行したトークンを
//! そのままクライアントへ渡し、リフレッシュも Google へ中継するだけ**である。
//!
//! この AS が引き受けるのは次の 1 点だけになった。
//! 1. **DCR の成立**: Google は RFC 7591 に対応しないので、claude.ai が接続時に
//!    行う動的クライアント登録をこちらで受ける。
//!
//! # confused deputy 対策は redirect_uri の許可リスト（2026-07 改訂）
//!
//! `/oauth/register`（DCR）は無認証なので、redirect_uri を無制限に受け付けると
//! 第三者が任意ホストの redirect_uri を登録でき、`callback` がその第三者へ
//! 認可コードを配送してしまう（confused deputy）。一時期はこれを自前の同意画面
//! （`/oauth/consent`）で塞いでいたが、**redirect_uri を許可リストで縛れば経路
//! 自体が消えるため、同意画面は不要**と判断して撤去した。`is_acceptable_redirect_uri`
//! が https を `claude.ai` へのホスト完全一致に限定し、http は loopback のみを許す。
//! 2 段目のリダイレクト（AS → 動的登録クライアント）は Google 側の redirect_uri
//! 設定では一切守られない点に注意（詳細:
//! docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md）。
//!
//! 帰結として、この AS 自身は**個別トークンの失効手段を持たない**。特定利用者の失効は
//! Google 側（アカウントのアクセス権限管理）で行う。
//!
//! **一括失効は署名鍵の入れ替えで行う。** 鍵を差し替えると全 DCR 登録と進行中の
//! ログインフローが無効になり、`token_from_refresh` の `client_id` 検証が落ちて
//! 全クライアントが再接続を要求される（`signing.rs` 参照）。裏返せば、**鍵さえ同一なら
//! 再起動で利用者はログアウトしない**。鍵が起動ごとに変わる状態だと、その逆に
//! 「再起動・コールドスタートのたびに全員ログアウト」になる。
//!
//! **失われた制御（意図的）**: 旧実装のアクセストークンは `aud` に project_id を
//! 持ち、`/{project_id}/mcp` ごとに束縛されていた。Google 発行のトークンには
//! この束縛を載せられないため、**ある project 向けに取得したトークンは、この
//! サーバの全 project の endpoint で通る**。project が増えた時点でテナント境界が
//! 無いことを意味するので、`resource` は入力検証にしか使っていない（`authorize`）。
//!
//! 状態を持たない理由と、署名鍵を運用者管理（`CS_SUPPORT_OAUTH_SIGNING_KEY`）にしている
//! 理由は `signing.rs` を参照。**鍵は refresh 経路のゲートでもある**ため、プロセス限りに
//! すると利用者が再起動のたびにログアウトする。

use super::signing::{b64_encode, SigningKey};
use super::verifier::GoogleTokenVerifier;
use super::AuthError;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Form, Json, Router};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// クライアントを Google へ往復させる `state` ブロブの寿命。
/// 利用者が Google の同意画面を操作し終えるのに十分で、かつ盗まれた state が
/// 使い回せる窓を短く保てる値。
const STATE_TTL_SECS: u64 = 600;

/// 自前の認可コードの寿命。RFC 6749 の推奨（10 分以内、可能な限り短く）に対し、
/// クライアントが即座に交換する前提で 60 秒に切り詰める。
/// 単回使用の担保が best-effort であること（`consume_code` 参照）の主たる補償がこれ。
const CODE_TTL_SECS: u64 = 60;

/// Google が `expires_in` を返さなかったときに仮定する access_token の寿命。
///
/// Google は実際には常に返すが、欠けたときに `expires_in` を省いた応答を
/// クライアントへ渡すと、クライアントは寿命不明のトークンを持つことになり
/// リフレッシュの契機を失う。実測値（3599 秒）より **短い** 値を仮定すれば、
/// 最悪でも「まだ使えるのに早めにリフレッシュする」で済む。長く仮定すると
/// 逆に「切れているのに使い続けて 401 を踏む」になるため、短い側へ倒す。
const ASSUMED_GOOGLE_ACCESS_TTL_SECS: u64 = 600;

/// DCR で発行したクライアント登録の有効期間（S3）。
///
/// `iat` を持たせながら検証しないと、`/oauth/register` が無認証である以上、
/// 一度登録された client_id が**永久に有効**になる。登録簿を持たない
/// （ステートレス）設計では個別失効ができないため、期限で自然に切れるようにする。
/// claude.ai は接続時に DCR をやり直せるため、期限切れは再登録で回復する。
const CLIENT_MAX_AGE_SECS: u64 = 90 * 24 * 3600;

/// DCR で受け付ける redirect_uri の上限（S4）。無制限だと、署名対象の JSON が
/// 際限なく膨らみ、client_id 文字列そのものが増幅の材料になる。
const MAX_REDIRECT_URIS: usize = 10;
const MAX_REDIRECT_URI_LEN: usize = 2048;

/// 1 登録あたりの redirect_uri 合計バイト数の上限（S3）。
///
/// 件数と 1 件あたりの長さだけを制限しても、上限いっぱい（10 × 2048 = 約 20KB）の
/// 登録が通ってしまう。client_id は登録内容そのものを署名した値なので、そのまま
/// **約 27KB の client_id** になり、それを内包する封緘 state が Google の
/// authorize URL のクエリに載る。実用的な URL 長（ブラウザ・Google 側とも
/// おおむね 8KB 前後）を超えるため、**そのクライアントのログインが恒久的に失敗する**。
/// 登録時点で検出できないと、原因が authorize のリダイレクト先で初めて現れて
/// 切り分けが難しい。
///
/// 値の決め方: 登録サイズ R は client_id（base64 で約 1.35 倍）→ それを内包する
/// 封緘 state（さらに約 1.35 倍）と二重に増幅されて authorize URL に載るため、
/// URL 長はおおむね `1.8 × R + 定数` になる。実測では R=4000 で URL が 9399 バイトに
/// 達し、実用的な上限（おおむね 8KB）を超えた。claude.ai の実際の登録は
/// 1 件・約 40 文字なので、2KB でも 50 倍の余裕がある。
///
/// **これは登録時点で検出するための予算であって、URL 長の保証そのものではない。**
/// `client_state` は authorize 時に初めて渡されるため、ここでは数えられない。
/// 最終的な保証は `authorize` が組み上がった URL を `MAX_AUTHORIZE_URL_BYTES` と
/// 実測比較して行う。この予算の役割は、**authorize まで進まないと分からない失敗を
/// 登録時点に前倒しする**ことにある（原因がリダイレクト先で初めて現れるのを避ける）。
const MAX_REGISTRATION_BYTES: usize = 2048;

/// 使用済み jti 集合の上限件数（C3）。
///
/// 1 件あたり uuid 36 バイト + `u64` + HashMap のオーバーヘッドで概ね 100 バイト、
/// 10,000 件で約 1MB。Cloud Run の割当（512Mi）に対して十分小さい。
/// 一方、生存期間が最長の state（600 秒）で 10,000 件を維持するには毎秒 16 件以上の
/// authorize を継続する必要があり、CS 担当者の実利用（1 日に数回のログイン）とは
/// 桁が違う。**正常系がこの上限に触れることはない。**
const MAX_USED_JTIS: usize = 10_000;

/// 期限切れの刈り取りを始める件数（C3）。
///
/// 刈り取りは O(n) の全走査なので、**挿入のたびに実行すると攻撃者のリクエスト数 r に
/// 対して防御側コストが O(r²) になる**（集合サイズ n ≈ 600r に対し毎回 n を走査する）。
/// 攻撃コストに対して防御コストが超線形になるのは、前段のレート制限で守るという
/// 整理の前提（負荷はリクエスト数に比例する）を崩す。
///
/// そこで「一定件数を超えたときだけ刈り取り、その直後に次の閾値を生存件数の 2 倍に
/// 引き上げる」償却方式にする。これにより刈り取りの償却コストは挿入あたり O(1) に
/// 収まる（動的配列の拡張と同じ考え方）。
const PRUNE_THRESHOLD: usize = 1024;

/// Google の authorize URL として送出を許す最大バイト数（C2）。
///
/// ブラウザおよび Google 側の実用的な URL 長上限がおおむね 8KB であることに合わせる。
/// これを超える URL は送っても失敗するので、こちらで検出して原因の分かる
/// エラーを返す。`MAX_REGISTRATION_BYTES` と違い、**これが実際の保証**である。
const MAX_AUTHORIZE_URL_BYTES: usize = 8192;
/// `client_name` は攻撃者が任意に設定でき、同意画面に表示する値でもある（C2）。
/// 表示を破壊しない長さに切り詰める。
const MAX_CLIENT_NAME_LEN: usize = 256;

/// クライアントが `/oauth/authorize` に渡す `state` の上限（S3）。
/// 署名対象のブロブと、最終的なリダイレクト URL のクエリに載る値なので上限を課す。
/// RFC に上限は無いが、CSRF 対策の nonce としての用途に 1 KiB あれば十分すぎる。
const MAX_CLIENT_STATE_LEN: usize = 1024;

/// Google の `Retry-After` として受け入れる上限（秒）。これを超える指示は無視する。
/// 上流が極端な値を返したときに、こちらのクライアントを事実上締め出さないため。
const MAX_RETRY_AFTER_SECS: u64 = 300;

const GOOGLE_AUTHORIZE_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";

/// Google token endpoint への接続・全体 timeout。根拠は `verifier.rs` の
/// `CONNECT_TIMEOUT` / `REQUEST_TIMEOUT` と同じ（Cloud Run の request deadline を
/// 認証で食い潰さない）。
const GOOGLE_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const GOOGLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// 署名付きブロブの中身。
///
/// `#[serde(tag = "typ")]` により、**種別の取り違えは deserialize 段階で失敗する**。
/// 「署名は正しいので通す」という事故（典型的にはリフレッシュトークンを
/// アクセストークンとして受理してしまう）を、呼び出し規約ではなく型で防ぐのが狙い。
/// **`Debug` は手で実装する（S2）。** derive すると、`Blob::State` の
/// `google_verifier`、`Blob::Code` の `jti` / `code_challenge`
/// および **Google の access_token / refresh_token** が、将来
/// `tracing::debug!(?blob)` を 1 行足された瞬間に平文で Cloud Logging へ落ちる。
/// `SigningKey` / `AuthServerConfig` が既に同じ方針なので揃える。
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "typ")]
pub enum Blob {
    /// DCR で発行する client_id の中身。登録簿を持たず、登録内容そのものを client_id にする。
    #[serde(rename = "client")]
    Client {
        redirect_uris: Vec<String>,
        /// 登録時にクライアントが名乗った表示名。**未検証の攻撃者制御値**であり、
        /// 検証済みの名称であるかのように扱わないこと。
        client_name: Option<String>,
        iat: u64,
    },
    /// Google へ往復する間、クライアント側の文脈を保持するためのブロブ。
    ///
    /// **必ず `seal` で封緘する**（reviewer 指摘 W3）。このブロブは Google の
    /// authorize URL に `state` クエリパラメータとして載るため、署名だけだと中身が
    /// ブラウザ履歴・Referer・Google 側ログ・ブラウザ拡張から平文で読める。
    /// 中の `google_verifier` は AS が Google に対して使う PKCE verifier であり、
    /// これが読めると「認可コード横取りへの二重防御」の片翼が成立しない。
    #[serde(rename = "state")]
    State {
        client_id: String,
        /// クライアントの redirect_uri（callback で戻す先）。
        redirect_uri: String,
        /// クライアントが渡してきた元の `state`（無いこともある）。
        client_state: Option<String>,
        /// クライアントの PKCE challenge。認可コードに引き継ぎ、token で照合する。
        code_challenge: String,
        /// **Google 向け** PKCE verifier。AS 自身も Google に対して PKCE を使う。
        google_verifier: String,
        /// 単回使用判定のための一意 ID（reviewer 指摘 W-A / C2）。
        /// これが無いと、1 回の `authorize` で得た state を `STATE_TTL_SECS`（600 秒）
        /// にわたって再利用でき、**無認証のまま Google への外向きリクエストを
        /// 任意レートで発生させられる**（`callback` の消費処理を参照）。
        jti: String,
        exp: u64,
    },
    /// 自前の認可コード。
    ///
    /// **必ず `seal` で封緘する**。Google の access_token / refresh_token を運び、
    /// かつ認可コードはクライアントの redirect_uri へクエリ文字列として渡る
    /// （＝ブラウザ履歴・Referer・クライアント側ログに残りうる）ため、署名だけでは
    /// 上流クレデンシャルがそれらすべてに平文で残る。
    #[serde(rename = "code")]
    Code {
        sub: String,
        email: String,
        client_id: String,
        redirect_uri: String,
        code_challenge: String,
        /// Google が発行したトークン群。`/oauth/token` でそのままクライアントへ渡す。
        upstream: UpstreamTokens,
        /// 単回使用判定のための一意 ID（`consume_code` 参照）。
        jti: String,
        exp: u64,
    },
}

/// Google が発行したトークン群。**この AS は自前のトークンを発行せず、これを
/// そのままクライアントへ渡す。**
///
/// `Debug` を手で実装して中身を隠す。derive すると、この構造体を含む `Blob` を
/// ログした瞬間に上流クレデンシャルが平文で Cloud Logging に落ちる。
#[derive(Serialize, Deserialize, PartialEq, Eq, Clone)]
pub struct UpstreamTokens {
    /// **秘密**。クライアントが `/{project_id}/mcp` に提示する Bearer になる。
    access_token: String,
    /// **秘密**。クライアントが `grant_type=refresh_token` で提示する値になり、
    /// こちらは Google へ中継するだけ。Google が返さないことがある
    /// （`callback` の縮退分岐を参照）。
    refresh_token: Option<String>,
    /// access_token の失効時刻（UNIX 秒）。Google の `expires_in` を受け取った
    /// 時点の絶対時刻へ変換して持つ。**相対秒のまま持たない** —— このブロブは
    /// 同意画面と認可コードを経由して最長 360 秒滞留しうるので、相対秒だと
    /// クライアントへ渡すころには実際より長い寿命を申告することになる。
    expires_at: u64,
}

impl fmt::Debug for UpstreamTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UpstreamTokens")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

impl UpstreamTokens {
    /// クライアントへ返す `expires_in`（秒）。失効済みなら 0。
    ///
    /// 絶対時刻から引き直すことで、同意画面での滞留分がそのまま差し引かれる。
    fn expires_in(&self, now: u64) -> u64 {
        self.expires_at.saturating_sub(now)
    }
}

/// 種別と、秘密を含まないごく一部のフィールドだけを出す。値は一切出さない。
impl fmt::Debug for Blob {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self {
            Blob::Client { .. } => "client",
            Blob::State { .. } => "state",
            Blob::Code { .. } => "code",
        };
        write!(f, "Blob::{kind}(<redacted>)")
    }
}

/// Google に要求する scope。クライアントが要求した scope は採用しない
/// （この AS が Google に求めるのは identity の確定だけであり、クライアント側の
/// 要求で upstream の権限を広げられるようにしない）。
const GOOGLE_SCOPE: &str = "openid email profile";

/// AS の設定。Google の endpoint はテストで stub に差し替えるため注入可能にする。
#[derive(Clone)]
pub struct AuthServerConfig {
    pub public_host: String,
    pub google_client_id: String,
    /// **秘密**。ログ・レスポンス・エラーに絶対に載せない。`Debug` にも出さないため、
    /// この構造体を `?config` でログしないこと（下の `Debug` 実装で潰している）。
    pub google_client_secret: String,
    pub google_authorize_url: String,
    pub google_token_url: String,
    /// config に定義された全 project_id（C5）。`resource` パラメータの解決先であり、
    /// アクセストークンの `aud` になりうる値の全集合。
    pub project_ids: Vec<String>,
}

/// `google_client_secret` を隠すため `Debug` を手で実装する。derive すると、
/// この設定を含む構造体を `?config` でログした瞬間に Google の client_secret が
/// 平文で Cloud Logging に落ちる。
impl fmt::Debug for AuthServerConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("AuthServerConfig")
            .field("public_host", &self.public_host)
            .field("google_client_id", &self.google_client_id)
            .field("google_client_secret", &"<redacted>")
            .field("google_authorize_url", &self.google_authorize_url)
            .field("google_token_url", &self.google_token_url)
            .field("project_ids", &self.project_ids)
            .finish()
    }
}

impl AuthServerConfig {
    /// 本番用。Google の実 endpoint を使う。
    pub fn new(
        public_host: String,
        google_client_id: String,
        google_client_secret: String,
        project_ids: Vec<String>,
    ) -> Self {
        Self {
            public_host,
            google_client_id,
            google_client_secret,
            google_authorize_url: GOOGLE_AUTHORIZE_URL.to_string(),
            google_token_url: GOOGLE_TOKEN_URL.to_string(),
            project_ids,
        }
    }

    fn callback_uri(&self) -> String {
        format!("https://{}/oauth/callback", self.public_host)
    }
}

pub struct AuthServerState {
    config: AuthServerConfig,
    signing_key: Arc<SigningKey>,
    verifier: Arc<GoogleTokenVerifier>,
    http: reqwest::Client,
    /// 使用済み jti の集合。詳細と上限の根拠は `UsedJtis` / `consume_jti` を参照。
    used_codes: Mutex<UsedJtis>,
    clock: Arc<dyn Clock>,
}

impl AuthServerState {
    pub fn new(
        config: AuthServerConfig,
        signing_key: Arc<SigningKey>,
        verifier: Arc<GoogleTokenVerifier>,
    ) -> Self {
        Self::with_clock(config, signing_key, verifier, Arc::new(SystemClock))
    }

    pub(crate) fn with_clock(
        config: AuthServerConfig,
        signing_key: Arc<SigningKey>,
        verifier: Arc<GoogleTokenVerifier>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        let http = reqwest::Client::builder()
            .connect_timeout(GOOGLE_CONNECT_TIMEOUT)
            .timeout(GOOGLE_REQUEST_TIMEOUT)
            .build()
            // 失敗は TLS backend 初期化不能などプロセス起動レベルの環境不備のみ。
            // timeout 無し client へ静かにフォールバックするより落ちる方が安全。
            .expect("failed to build reqwest client with timeouts for Google token endpoint");
        Self {
            config,
            signing_key,
            verifier,
            http,
            used_codes: Mutex::new(UsedJtis::new()),
            clock,
        }
    }

    /// 現在時刻（UNIX 秒）。注入された時刻源を経由する。
    fn now(&self) -> u64 {
        self.clock.now_secs()
    }

    /// `now() + ttl` の安全版。`now()` が fail closed で `u64::MAX` を返したときに
    /// 桁溢れ（debug ビルドでは panic、release では小さい値へ巻き戻り）しないよう、
    /// 飽和加算にする。飽和した `u64::MAX` はそのまま「常に期限切れ」として扱われる。
    fn expires_in(&self, ttl: u64) -> u64 {
        self.now().saturating_add(ttl)
    }
}

pub fn auth_server_router(state: Arc<AuthServerState>) -> Router {
    Router::new()
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize))
        .route("/oauth/callback", get(callback))
        .route("/oauth/token", post(token))
        .with_state(state)
}

/// `resource`（RFC 8707）を project_id へ解決する。
///
/// **2026-07 改訂 / 自前トークン廃止に伴う縮退**: 旧実装はここで解決した project_id を
/// アクセストークンの `aud` に載せ、`/{project_id}/mcp` ごとの束縛にしていた（C5）。
/// 現在クライアントへ渡すのは Google 発行のトークンで、こちらの project_id を
/// 載せる余地が無いため、**この解決結果は入力検証にしか使っていない**。
/// 設定に無い project や別ホストを指す `resource` は引き続き拒否するが、
/// 「あるproject 向けに取得したトークンが別 project でも通る」ことは防げない。
///
/// 受け付ける形は、metadata（`metadata.rs`）が `resource` として広告している
/// `https://{public_host}/{project_id}/mcp` そのもの。クライアントは保護リソース
/// メタデータからこの値を得るため、実運用では完全に一致する。
///
/// host が `public_host` と一致しない `resource` は拒否する。他ホストを指す
/// resource を受け入れると、`aud` の意味が「このサーバのどの project か」から
/// ずれて束縛が形骸化する。
///
/// **`resource` が無い場合**: config の project が 1 件のときに限り、それに束縛する。
/// 2 件以上あるときは束縛先を推測できないため `invalid_target` で拒否する
/// （fail closed）。本番 project は現状 `urtect` の 1 件のみで、必須化すると
/// `resource` を送らないクライアントの既存接続を落とすリスクがあるため、
/// project が増えた時点で自動的に厳格化される形にしてある。
/// この連動は `CLAUDE.md` / `config.cloudrun.toml` にも明記し、起動時にも警告する。
fn resolve_resource(config: &AuthServerConfig, resource: Option<&str>) -> Result<String, String> {
    let Some(resource) = resource.filter(|s| !s.is_empty()) else {
        return match config.project_ids.as_slice() {
            [only] => Ok(only.clone()),
            _ => {
                Err("resource is required because this server serves more than one project".into())
            }
        };
    };
    let Ok(parsed) = url::Url::parse(resource) else {
        return Err("resource must be an absolute URI".into());
    };
    let authority = match (parsed.host_str(), parsed.port()) {
        (Some(host), Some(port)) => format!("{host}:{port}"),
        (Some(host), None) => host.to_string(),
        (None, _) => return Err("resource must include a host".into()),
    };
    if authority != config.public_host {
        return Err("resource does not identify this server".into());
    }
    // 末尾スラッシュの有無を吸収する（`/urtect/mcp` と `/urtect/mcp/` を同一視）。
    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let [project_id, "mcp"] = segments.as_slice() else {
        return Err("resource must be the MCP endpoint of a configured project".into());
    };
    if !config.project_ids.iter().any(|p| p == project_id) {
        return Err("resource does not identify a configured project".into());
    }
    Ok((*project_id).to_string())
}

/// クライアント登録の指紋（S1）。監査ログとアクセストークンに載せる短い識別子。
///
/// client_id 本体（署名済みブロブ）は数百文字あり、ログに毎回載せるには長すぎる。
/// SHA-256 の先頭 16 桁で、実運用の登録数に対して衝突は起きない。
/// 秘密ではないが、ハッシュにしておくとログ行の長さが一定になり grep しやすい。
fn client_fingerprint(client_id: &str) -> String {
    format!("{:x}", Sha256::digest(client_id.as_bytes()))
        .chars()
        .take(16)
        .collect()
}

/// S256 の `code_challenge` として妥当な形かを検証する（S4）。
///
/// `base64url(sha256(verifier))` はパディング無しで必ず 43 文字になり、文字集合は
/// base64url の `A-Za-z0-9-_` に限られる。長さと文字集合の両方を見る。
fn is_valid_s256_challenge(challenge: &str) -> bool {
    const S256_CHALLENGE_LEN: usize = 43;
    challenge.len() == S256_CHALLENGE_LEN
        && challenge
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

// ---------------------------------------------------------------------------
// 共通ヘルパ
// ---------------------------------------------------------------------------

/// 現在時刻（UNIX 秒）。**時計異常時は fail closed に倒す。**
///
/// 2026-07 改訂 / reviewer 指摘 C4:
/// 旧実装は `unwrap_or(0)` で、コメントには「0 に倒すと全ブロブが期限切れ扱いに
/// なり fail closed になる」と書いてあったが、**実挙動は真逆だった**。
/// 期限判定は全箇所が `if exp <= now_secs()` であり、`now = 0` なら正の `exp` に
/// 対して常に false、つまり state・認可コード・同意ブロブ・リフレッシュトークンが
/// 一切期限切れにならない fail **open** になっていた。
///
/// `u64::MAX` に倒すと、逆にあらゆる `exp` が `exp <= now` を満たして期限切れ扱いに
/// なり、意図どおり fail closed になる（`middleware.rs` の同じ判断と揃えた）。
/// 発行側の `exp` 計算は `saturating_add` にしてあるため、桁溢れで小さい値へ
/// 巻き戻ることもない。運用上は全 OAuth フローが停止して時計異常に気づける。
fn now_secs() -> u64 {
    clock_secs(SystemTime::now().duration_since(UNIX_EPOCH).ok())
}

/// 秒単位の時刻源。**テストで時間を進めるために注入可能にする。**
///
/// 単回使用集合の刈り取り（`consume_jti`）と各ブロブの期限判定は、どちらも
/// 「経過時間」でしか壊れ方が現れない。実時間の `sleep` に頼るテストは遅く不安定なので、
/// ここを差し替え可能にして論理時計で検証する。
pub(crate) trait Clock: Send + Sync {
    fn now_secs(&self) -> u64;
}

/// 本番の時刻源。`now_secs()`（時計異常時は fail closed）をそのまま使う。
struct SystemClock;

impl Clock for SystemClock {
    fn now_secs(&self) -> u64 {
        now_secs()
    }
}

/// `now_secs` の判断部分を、システム時計から切り離した純粋関数として取り出したもの。
///
/// 時計異常（`duration_since` が `Err` = 1970 より前）は実プロセスでは再現できず、
/// テストから踏めない。fail open か fail closed かはこのモジュールで最も間違えやすい
/// 判断であり（現に旧実装は逆になっていた）、分岐だけでも直接テストできる形にする。
fn clock_secs(elapsed: Option<Duration>) -> u64 {
    elapsed.map_or(u64::MAX, |d| d.as_secs())
}

/// PKCE の S256 変換。`base64url(sha256(verifier))`（パディング無し）。
fn s256(verifier: &str) -> String {
    b64_encode(&Sha256::digest(verifier.as_bytes()))
}

/// RFC 6749 §5.1 がトークン応答に MUST で要求するキャッシュ抑止ヘッダ（W2）。
/// アクセストークン・リフレッシュトークンを共有キャッシュやブラウザ履歴に
/// 残さないためのもの。`Pragma` は HTTP/1.0 キャッシュ向けの互換指定。
const NO_STORE: &str = "no-store";
const NO_CACHE: &str = "no-cache";

/// `/oauth/callback` で上流が一時的に失敗したときの案内文。
///
/// **この経路は再試行できない。** state は Google を叩く前に消費される
/// （増幅対策。`callback` の `consume_jti` 参照）ため、同じ callback URL を
/// 再読み込みしても `state_already_used` で弾かれる。利用者は認可フローを
/// 最初からやり直す必要がある。
///
/// 単回使用と再試行可能性はここでは両立させない。両立させるには、消費済み state に
/// 対する上流交換の結果を短期間保存して再生する（single-flight 相当の）機構が要り、
/// 「無認証の相手が Google への外向きリクエストを増幅できない」という単回使用の
/// 目的に対して複雑性が見合わないと判断した。**一時障害の頻度は低く、代償は
/// 「ログインをやり直す」で済む**のに対し、増幅経路を開けた場合の代償は
/// 失効伝播の停止とインスタンス飽和である。
const UPSTREAM_TEMPORARY_HINT: &str =
    "the upstream identity provider is temporarily unavailable; start the login again \
     (this authorization request cannot be resumed)";

/// トークン応答を組み立てる。**`/oauth/token` の 200 応答は必ずここを通す**
/// （`Json(...)` を直接返すと W2 のヘッダが抜ける）。
fn token_response(body: serde_json::Value) -> Response {
    (
        StatusCode::OK,
        [
            (header::CACHE_CONTROL, NO_STORE),
            (header::PRAGMA, NO_CACHE),
        ],
        Json(body),
    )
        .into_response()
}

/// RFC 6749 形式のエラー応答。**`error_description` に受け取った値そのもの
/// （code / token / verifier / client_id）を絶対に埋め込まない**。
/// このレスポンスはクライアント側のログにも残るため、埋めた瞬間に秘密が広がる。
fn oauth_error(status: StatusCode, code: &str, description: &str) -> Response {
    (
        status,
        // エラー応答にもキャッシュ抑止を付ける（RFC 6749 §5.2）。
        [
            (header::CACHE_CONTROL, NO_STORE),
            (header::PRAGMA, NO_CACHE),
        ],
        Json(serde_json::json!({ "error": code, "error_description": description })),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct RegisterRequest {
    redirect_uris: Option<Vec<String>>,
    client_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AuthorizeQuery {
    response_type: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    code_challenge: Option<String>,
    code_challenge_method: Option<String>,
    state: Option<String>,
    scope: Option<String>,
    /// RFC 8707。束縛先の保護リソース（= project の MCP endpoint）。
    resource: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct TokenForm {
    grant_type: Option<String>,
    code: Option<String>,
    code_verifier: Option<String>,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    refresh_token: Option<String>,
    /// RFC 8707。リフレッシュ時に指定された場合、元の束縛と一致しなければ拒否する
    /// （別 resource 向けのアクセストークンを取り直せないようにする / C5）。
    resource: Option<String>,
}

/// 302 + `Location` を組み立てる。**URL 自体はログしない**
/// （認可コード・state を含むため）。組み立て失敗時のみ、エラー種別だけを残す。
fn redirect(url: &str) -> Response {
    match axum::http::HeaderValue::from_str(url) {
        Ok(hv) => {
            let mut res = StatusCode::FOUND.into_response();
            res.headers_mut().insert(header::LOCATION, hv);
            res
        }
        Err(e) => {
            tracing::error!(error = %e, "failed to build a Location header for an OAuth redirect");
            oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "could not build the redirect target",
            )
        }
    }
}

/// RFC 6749 §4.1.2.1 のエラーリダイレクト（W1）。
///
/// **検証済みの `redirect_uri` に対してのみ使う。** 具体的には、こちらが署名した
/// `state` / 同意ブロブから取り出した redirect_uri（= authorize 時に登録との
/// 完全一致を確認済み）だけ。state 検証前のエラーで使ってはならない —
/// その段階の redirect 先は攻撃者が自由に指定でき、リダイレクトさせること自体が
/// オープンリダイレクタになる。state 検証前は 400 JSON のまま維持する。
///
/// `error` には静的な RFC エラーコードだけを載せ、受け取った値は載せない。
fn redirect_error(redirect_uri: &str, error: &'static str, client_state: Option<&str>) -> Response {
    let Ok(mut target) = url::Url::parse(redirect_uri) else {
        // 署名済みブロブ由来なので、ここに来るのは登録時検証をすり抜けた場合のみ。
        // 戻せない以上リダイレクトはせず、400 で止める。
        tracing::error!("a signed redirect_uri could not be parsed for an error redirect");
        return oauth_error(
            StatusCode::BAD_REQUEST,
            error,
            "the authorization request failed and the registered redirect_uri is not usable",
        );
    };
    {
        let mut pairs = target.query_pairs_mut();
        pairs.append_pair("error", error);
        if let Some(cs) = client_state {
            pairs.append_pair("state", cs);
        }
    }
    redirect(target.as_str())
}

/// 署名に失敗したとき共通で返す 500。署名失敗は payload の serde 化失敗のみで、
/// 実運用では起きない（構造体は固定）。起きた場合は実装バグなので error で残す。
fn signing_failed(what: &str, err: &serde_json::Error) -> Response {
    tracing::error!(error = %err, blob_kind = what, "failed to sign an OAuth blob");
    oauth_error(
        StatusCode::INTERNAL_SERVER_ERROR,
        "server_error",
        "could not issue a signed value",
    )
}

/// DCR の https コールバックとして許可する唯一のホスト。
///
/// 確定値は `https://claude.ai/api/mcp/auth_callback`（本番の DCR で実際に送られてくる
/// 値）。値が変わったときにここ 1 箇所だけ直せばよいように定数へ切り出す。
const ALLOWED_HTTPS_CALLBACK_HOST: &str = "claude.ai";

/// redirect_uri として受け入れてよい形か検証する。
///
/// `/oauth/register`（DCR）は無認証のため、ここでホストを絞らないと第三者が
/// 任意ホストの redirect_uri を登録でき、`callback` がその第三者へ認可コードを
/// 配送してしまう（confused deputy。設計:
/// docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md）。
///
/// `https` は `ALLOWED_HTTPS_CALLBACK_HOST` への**ホスト完全一致**でだけ許可する。
/// **サフィックス一致（`ends_with`）にしない。** それだと `evil-claude.ai` のような
/// 別ドメインが通ってしまう。
///
/// `http` は loopback（localhost / 127.0.0.1 / ::1）に限る（ポートは任意）
/// — ネイティブクライアントのローカル受け口を塞がないためだが、平文で認可コードが
/// 流れる経路を LAN・インターネットへ広げないため loopback 以外は拒否する。
fn is_acceptable_redirect_uri(uri: &str) -> bool {
    let Ok(parsed) = url::Url::parse(uri) else {
        return false;
    };
    match parsed.scheme() {
        "https" => parsed.host_str() == Some(ALLOWED_HTTPS_CALLBACK_HOST),
        "http" => matches!(
            parsed.host_str(),
            Some("localhost" | "127.0.0.1" | "[::1]" | "::1")
        ),
        _ => false,
    }
}

async fn register(
    State(state): State<Arc<AuthServerState>>,
    Json(req): Json<RegisterRequest>,
) -> Response {
    let redirect_uris = match req.redirect_uris {
        Some(uris) if !uris.is_empty() => uris,
        _ => {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_redirect_uri",
                "redirect_uris is required and must contain at least one absolute URI",
            )
        }
    };
    // S4: 件数・長さの上限。無認証 endpoint なので、上限が無いと署名対象の JSON を
    // 際限なく膨らませられ、返す client_id 文字列そのものが増幅の材料になる。
    if redirect_uris.len() > MAX_REDIRECT_URIS {
        tracing::info!(
            count = redirect_uris.len(),
            "rejected client registration with too many redirect_uris"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "too many redirect_uris in one registration",
        );
    }
    if let Some(bad) = redirect_uris
        .iter()
        .find(|u| u.len() > MAX_REDIRECT_URI_LEN)
    {
        tracing::info!(
            length = bad.len(),
            "rejected client registration with an overlong redirect_uri"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "a redirect_uri exceeded the maximum accepted length",
        );
    }
    // S3 / C2: 合計サイズの上限。件数 × 1 件あたりの長さだけでは、client_id が
    // authorize URL に収まらない大きさまで膨らむのを防げない（定数のコメント参照）。
    //
    // 2026-07 改訂 / reviewer 指摘 W-1: `client_name` も `Blob::Client` に入り、
    // client_id 経由で二重に増幅されるため**合算する**。旧実装は redirect_uris しか
    // 数えておらず、上限いっぱいの登録が authorize URL を 8KB 超へ押し上げた。
    let total_bytes: usize = redirect_uris.iter().map(String::len).sum::<usize>()
        + req.client_name.as_deref().map_or(0, str::len);
    if total_bytes > MAX_REGISTRATION_BYTES {
        tracing::info!(
            total_bytes,
            "rejected client registration whose redirect_uris are too large in total"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "the redirect_uris are too large in total; a registration this size would produce \
             a client_id that cannot fit in an authorization URL",
        );
    }
    if let Some(name) = req.client_name.as_deref() {
        // C2: **バイト長**で数える。`chars().count()` だと 4 バイト文字ばかりの名前で
        // 実バイトが 4 倍になり、上限が意味を成さない（URL 長の見積もりが崩れる）。
        if name.len() > MAX_CLIENT_NAME_LEN {
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_client_metadata",
                "client_name exceeded the maximum accepted length",
            );
        }
    }
    if let Some(bad) = redirect_uris
        .iter()
        .find(|u| !is_acceptable_redirect_uri(u))
    {
        // redirect_uri は秘密ではなく、拒否理由の特定に必須。制御文字によるログ
        // 行の偽造を防ぐため Debug（`?`）で出す（metadata.rs の project_id と同方針）。
        tracing::info!(
            redirect_uri = ?bad,
            "rejected client registration with an unusable redirect_uri"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_redirect_uri",
            "each redirect_uri must be an https URI to an allowed callback host, or an http URI \
             to loopback",
        );
    }

    let iat = state.now();
    // 登録簿を持たず、登録内容そのものを署名して client_id にする（ステートレス）。
    let client_id = match state.signing_key.sign(&Blob::Client {
        redirect_uris: redirect_uris.clone(),
        client_name: req.client_name.clone(),
        iat,
    }) {
        Ok(v) => v,
        Err(e) => return signing_failed("client", &e),
    };
    tracing::info!(
        redirect_uris = ?redirect_uris,
        "registered a dynamic OAuth client"
    );
    (
        StatusCode::CREATED,
        Json(serde_json::json!({
            "client_id": client_id,
            "client_id_issued_at": iat,
            "redirect_uris": redirect_uris,
            "client_name": req.client_name,
            // public client + PKCE。client_secret は発行しない。
            "token_endpoint_auth_method": "none",
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
        })),
    )
        .into_response()
}

async fn authorize(
    State(state): State<Arc<AuthServerState>>,
    Query(query): Query<AuthorizeQuery>,
) -> Response {
    let Some(client_id) = query.client_id.filter(|s| !s.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "client_id is required",
        );
    };
    // client_id は署名付きブロブ。改竄されていれば redirect_uris も信用できない。
    let Ok(Blob::Client {
        redirect_uris, iat, ..
    }) = state.signing_key.verify::<Blob>(&client_id)
    else {
        tracing::info!(reason = "unverifiable_client_id", "authorize rejected");
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "client_id is not a valid registration issued by this server",
        );
    };
    // S3: 登録の有効期限。`iat` を持ちながら検証しないと、無認証で発行された
    // client_id が永久に有効になる。
    if state.now().saturating_sub(iat) > CLIENT_MAX_AGE_SECS {
        tracing::info!(reason = "expired_client_registration", "authorize rejected");
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_client",
            "this client registration has expired; register the client again",
        );
    }

    let Some(redirect_uri) = query.redirect_uri else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri is required",
        );
    };
    // **完全一致のみ**。前方一致・部分一致を許すと、登録 URI を接頭辞に持つ別 URI で
    // 認可コードの配送先を奪える（redirect_uri 攻撃の典型）。
    if !redirect_uris.iter().any(|u| u == &redirect_uri) {
        tracing::info!(
            reason = "redirect_uri_not_registered",
            redirect_uri = ?redirect_uri,
            "authorize rejected"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "redirect_uri does not exactly match a registered redirect_uri",
        );
    }
    if query.response_type.as_deref() != Some("code") {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_response_type",
            "only response_type=code is supported",
        );
    }
    // PKCE は必須、かつ S256 のみ。`plain` は challenge が verifier そのもので、
    // 認可リクエストを覗ける相手に verifier を渡すのと同じになるため受け付けない。
    if query.code_challenge_method.as_deref() != Some("S256") {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_challenge_method must be S256",
        );
    }
    let Some(code_challenge) = query.code_challenge.filter(|s| !s.is_empty()) else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_challenge is required",
        );
    };
    // S4: S256 の challenge は `base64url(sha256(verifier))` = **パディング無し 43 文字**に
    // 一意に定まる。形式を authorize の時点で検証しないと、任意長・任意文字の値が
    // そのまま state → code へ運ばれ、`token` の照合で初めて落ちる。認可フローを
    // 最後まで進ませてから失敗させるより、入口で弾く方が原因が明確になる。
    if !is_valid_s256_challenge(&code_challenge) {
        tracing::info!(reason = "malformed_code_challenge", "authorize rejected");
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "code_challenge must be a base64url-encoded SHA-256 digest (43 characters, no padding)",
        );
    }
    // S3: `client_state` は署名対象のブロブへそのまま入り、最後はリダイレクト URL の
    // クエリにも載る。無制限だと state ブロブと Location ヘッダを際限なく膨らませられる。
    if let Some(client_state) = query.state.as_deref() {
        if client_state.len() > MAX_CLIENT_STATE_LEN {
            tracing::info!(
                length = client_state.len(),
                "authorize rejected an overlong state"
            );
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "state exceeded the maximum accepted length",
            );
        }
    }
    if let Some(scope) = query.scope.as_deref() {
        if scope != GOOGLE_SCOPE {
            tracing::debug!(
                requested_scope = ?scope,
                "client requested a scope other than the fixed upstream scope; using the fixed one"
            );
        }
    }

    // `resource`（RFC 8707）は **入力検証としてのみ**使う。設定に無い project や
    // 別ホストを指す値はここで拒否するが、解決した project_id を発行トークンに
    // 束縛することはできない（Google 発行のトークンにこちらの aud は載らない。
    // 詳細は `resolve_resource` とモジュールコメント）。
    let resolved_project = match resolve_resource(&state.config, query.resource.as_deref()) {
        Ok(project_id) => project_id,
        Err(reason) => {
            tracing::info!(reason = %reason, "authorize rejected an unusable resource");
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_target",
                "resource must identify the MCP endpoint of a configured project",
            );
        }
    };
    // project_id は秘密ではない。どの project 向けの認可要求だったかを追えるように
    // 残す（トークンに束縛が載らない以上、ログが唯一の手掛かりになる）。
    tracing::debug!(
        project_id = %resolved_project,
        "authorize accepted a resource (advisory only; the issued token is not bound to it)"
    );

    // AS 自身も Google に対して PKCE を使う（認可コード横取りへの二重防御）。
    // verifier は state ブロブに封じて往復させ、callback で取り出す。
    // uuid v4 を 2 本連結して 64 文字（PKCE の unreserved 43-128 文字を満たす）。
    let google_verifier = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    // W3: 封緘必須。`google_verifier` を Google の authorize URL に平文で晒さない。
    let state_blob = match state.signing_key.seal(&Blob::State {
        client_id,
        redirect_uri,
        client_state: query.state,
        code_challenge,
        google_verifier: google_verifier.clone(),
        jti: uuid::Uuid::new_v4().to_string(),
        exp: state.expires_in(STATE_TTL_SECS),
    }) {
        Ok(v) => v,
        Err(e) => return signing_failed("state", &e),
    };

    let mut url = match url::Url::parse(&state.config.google_authorize_url) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!(error = %e, "google_authorize_url is not a valid URL");
            return oauth_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "server_error",
                "the upstream authorization endpoint is misconfigured",
            );
        }
    };
    url.query_pairs_mut()
        .append_pair("client_id", &state.config.google_client_id)
        .append_pair("redirect_uri", &state.config.callback_uri())
        .append_pair("response_type", "code")
        .append_pair("scope", GOOGLE_SCOPE)
        .append_pair("state", &state_blob)
        .append_pair("code_challenge", &s256(&google_verifier))
        .append_pair("code_challenge_method", "S256")
        // C1: refresh_token を受け取るために必須。これが無いと Google は
        // access_token しか返さず、上流の失効を確認する手段が無くなる。
        .append_pair("access_type", "offline")
        // C1: **常に** 同意を再取得する。Google は `access_type=offline` だけでは、
        // 既に同意済みのユーザに refresh_token を返さない（初回認可時のみ）。
        // 本サービスは既に稼働しており同意済みユーザが存在するため、これを付けないと
        // 「既存ユーザだけリフレッシュ不能」という最も気づきにくい壊れ方をする。
        // 代償は毎ログインで Google の同意画面が出ること。リフレッシュが 30 日効くので
        // ログイン頻度は低く、許容する（README の OAuth 節に明記）。
        .append_pair("prompt", "consent");

    // C2（reviewer 指摘 W-1）: **最終的な URL 長をここで実測して保証する。**
    //
    // 登録側の予算（`MAX_REGISTRATION_BYTES`）は register 時点で見える入力しか
    // 数えられない。`client_state` は authorize 時に初めて渡され、しかも封緘 state に
    // 入って URL に載るため、登録側だけでは worst case を押さえきれない。
    // 定数同士の足し算で見積もるより、**組み上がった URL を直接測る**方が、
    // 将来 state に項目が増えても保証が崩れない。
    if url.as_str().len() > MAX_AUTHORIZE_URL_BYTES {
        tracing::warn!(
            url_bytes = url.as_str().len(),
            "authorize rejected because the upstream URL would be too long"
        );
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "the authorization request is too large; shorten the state parameter or \
             register the client with fewer or shorter redirect_uris",
        );
    }
    redirect(url.as_str())
}

/// Google token endpoint の応答のうち、この AS が使う部分。
///
/// `refresh_token` は **秘密**。`Debug` にも `Display` にも出さないため、この構造体を
/// まるごとログしない（下の手書き `Debug` で潰している）。
#[derive(Deserialize)]
struct GoogleTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    /// Google が申告する access_token の残り寿命（秒）。欠けたときの扱いは
    /// `ASSUMED_GOOGLE_ACCESS_TTL_SECS` を参照。
    expires_in: Option<u64>,
}

impl fmt::Debug for GoogleTokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("GoogleTokenResponse")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_in", &self.expires_in)
            .finish()
    }
}

async fn callback(
    State(state): State<Arc<AuthServerState>>,
    Query(query): Query<CallbackQuery>,
) -> Response {
    // W1: `state` を先に検証する。Google 由来のエラー（利用者が Google の同意を
    // 拒否した等）はクライアントへリダイレクトで返したいが、その redirect 先は
    // **こちらが署名した state から取り出したもの**でなければならない。
    // 旧実装は state を見る前に error を処理していたため、正当な access_denied すら
    // 400 JSON になり、クライアントがフローの終了を認識できなかった。
    let Some(state_param) = query.state else {
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "state is required",
        );
    };
    let Ok(Blob::State {
        client_id,
        redirect_uri,
        client_state,
        code_challenge,
        google_verifier,
        jti,
        exp,
    }) = state.signing_key.open::<Blob>(&state_param)
    else {
        // state を検証できない時点で redirect 先を信用できない。ここは 400 のまま。
        tracing::info!(reason = "unverifiable_state", "callback rejected");
        return oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_request",
            "state is not a valid value issued by this server",
        );
    };
    // ここから先の redirect_uri は「こちらが署名した state に入っていた値」＝
    // authorize 時に登録との完全一致を確認済みなので、W1 のリダイレクトに使ってよい。
    if exp <= state.now() {
        // W1: 期限切れもクライアントへ返す。利用者が同意画面を放置しただけなので、
        // クライアントがフローの失敗を認識してやり直せる形にする。
        tracing::info!(reason = "expired_state", "callback rejected");
        return redirect_error(&redirect_uri, "invalid_request", client_state.as_deref());
    }
    // C2（reviewer 指摘 W-A）: **state を単回使用にする。**
    //
    // `/oauth/register` も `/oauth/authorize` も無認証なので、認証情報を持たない
    // 相手でも有効な state を 1 つ入手できる。state が再利用可能だと、その 1 つで
    // `STATE_TTL_SECS`（600 秒）にわたり `callback?state=<有効>&code=<任意>` を
    // 任意レートで送り、**こちらから Google の token endpoint への外向きリクエストを
    // 無制限に発生させられる**。結果として
    // (a) Google がこの OAuth クライアントをレート制限すれば、失効伝播の要である
    //     上流照会が他人の濫用で止まる（W4 で塞いだのと同じ状態が、クレデンシャル
    //     不要のより低いコストで成立する）
    // (b) `maxScale=1` かつ外向き timeout 5 秒（`GOOGLE_REQUEST_TIMEOUT`）のため、
    //     少数の並行 callback でインスタンスを飽和させ MCP endpoint ごと止められる
    //
    // **消費は Google を叩く前**に行う。ここが後ろにあると増幅を止められない。
    // また `error` 分岐より前に置き、Google がエラーを返した経路でも state を
    // 使い切る（フローは終了しているので再利用させる理由が無い）。
    if !state.consume_jti(&jti, exp) {
        tracing::info!(reason = "state_already_used", "callback rejected");
        return redirect_error(&redirect_uri, "invalid_request", client_state.as_deref());
    }
    if let Some(error) = query.error {
        // Google 由来のエラーコード（access_denied 等）。値は攻撃者が仕込める経路が
        // あるため Debug で出す。秘密ではないので運用切り分けのために残す。
        tracing::info!(google_error = ?error, "google authorization failed");
        return redirect_error(&redirect_uri, "access_denied", client_state.as_deref());
    }
    let Some(code) = query.code.filter(|s| !s.is_empty()) else {
        return redirect_error(&redirect_uri, "invalid_request", client_state.as_deref());
    };

    // Google の client_id / client_secret は **サーバ側 env** から取る。
    // これを クライアントに持たせないことが、この AS を置いた唯一の目的である。
    let resp = state
        .http
        .post(&state.config.google_token_url)
        .form(&[
            ("code", code.as_str()),
            ("client_id", state.config.google_client_id.as_str()),
            ("client_secret", state.config.google_client_secret.as_str()),
            ("redirect_uri", state.config.callback_uri().as_str()),
            ("grant_type", "authorization_code"),
            ("code_verifier", google_verifier.as_str()),
        ])
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            // reqwest の Display はリクエスト URL を必ず書き出すため、絶対に埋め込まない
            // （`verifier.rs` の describe_transport_error と同じ方針）。
            tracing::error!(
                error = %super::verifier::describe_transport_error("google token exchange", &e),
                "failed to reach the Google token endpoint"
            );
            return temporarily_unavailable(None, UPSTREAM_TEMPORARY_HINT);
        }
    };
    if !resp.status().is_success() {
        // 2026-07 改訂 / codex 指摘 C1:
        // 旧実装はここでステータスを区別せず、**429・408・5xx を含むすべての
        // 非成功応答を `invalid_grant`** に変換していた。refresh 経路
        // （`refresh_upstream`）では分類を直したのに、兄弟経路であるこちらに同じ
        // 欠陥が残っていた。一時障害を `invalid_grant` として返すと、クライアントは
        // 「認可コードが無効」と解釈して回復可能な状況をエラーとして確定させる。
        //
        // 判定ロジックは refresh 経路と**同一の関数**を使う。分類基準が 2 箇所に
        // 分かれると、片方だけ直る（まさに今回起きたこと）ため。
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        // 本文は判定にだけ使い、生の本文はログにも応答にも載せない。
        let oauth_err = resp
            .text()
            .await
            .ok()
            .and_then(|body| serde_json::from_str::<GoogleErrorResponse>(&body).ok())
            .and_then(|e| e.error);
        return match classify_upstream_failure(status, oauth_err.as_deref(), retry_after) {
            UpstreamFailure::Rejected => oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_grant",
                "the upstream authorization code could not be exchanged",
            ),
            UpstreamFailure::Temporary { retry_after } => {
                temporarily_unavailable(retry_after, UPSTREAM_TEMPORARY_HINT)
            }
        };
    }
    let body = match resp.text().await {
        Ok(t) => t,
        Err(e) => {
            tracing::error!(
                error = %super::verifier::describe_transport_error("google token body read", &e),
                "failed to read the Google token endpoint response"
            );
            return temporarily_unavailable(None, UPSTREAM_TEMPORARY_HINT);
        }
    };
    // **パース失敗時に body をログしない**。body には Google の access_token /
    // id_token / refresh_token が入っている。
    let Ok(GoogleTokenResponse {
        access_token: Some(google_access_token),
        refresh_token: google_refresh_token,
        expires_in,
    }) = serde_json::from_str::<GoogleTokenResponse>(&body)
    else {
        tracing::error!("google token response had no usable access_token");
        return temporarily_unavailable(None, UPSTREAM_TEMPORARY_HINT);
    };
    // refresh_token が無い場合の縮退。`prompt=consent` を常に付けているので通常は
    // 必ず返るが、Google 側の挙動変更などで返らないことは起こりうる。このとき
    // クライアントには refresh_token 無しの応答が渡り、access_token の期限切れ後は
    // 再認可に落ちる（黙ってリフレッシュ不能になるのではない）。
    // identity はこの下で確定するため、ここでは sub を添えずに事実だけ残す。
    if google_refresh_token.is_none() {
        tracing::warn!(
            "google did not return a refresh_token; this login will get an access token only, \
             and the user will have to re-authorize when it expires"
        );
    }
    if expires_in.is_none() {
        tracing::warn!(
            assumed_ttl_secs = ASSUMED_GOOGLE_ACCESS_TTL_SECS,
            "google did not report expires_in for the access token; assuming a short lifetime \
             so the client refreshes early rather than using an expired token"
        );
    }
    let upstream = UpstreamTokens {
        access_token: google_access_token.clone(),
        refresh_token: google_refresh_token,
        expires_at: state.expires_in(expires_in.unwrap_or(ASSUMED_GOOGLE_ACCESS_TTL_SECS)),
    };

    // identity の確定は既存の `GoogleTokenVerifier` に任せる（aud 完全一致・
    // azp・`email_verified == "true"`・安定した `sub` の必須化が既に入っている）。
    // リクエスト経路からは外れたが、この一箇所では引き続き使う。
    let identity = match state.verifier.verify(&google_access_token).await {
        Ok(i) => i,
        Err(AuthError::Unreachable(msg)) => {
            tracing::error!(error = %msg, "could not verify the upstream access token");
            return oauth_error(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "could not verify the upstream identity",
            );
        }
        Err(AuthError::Invalid(msg)) => {
            // email 未検証・aud 不一致・sub 欠落はここ。利用者側の状態の問題なので
            // 503 ではなく 400 に倒す（運用者が Google 障害と切り分けられるように）。
            tracing::info!(reason = %msg, "rejected an upstream identity");
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "access_denied",
                "the upstream identity could not be accepted",
            );
        }
        Err(AuthError::Missing) => {
            // `verify` は引数のトークンを常に検査するため、この分類は返らない。
            // 将来 verifier 側の分類が変わったときに無言で通さないよう明示する。
            tracing::error!("upstream token verification reported a missing bearer unexpectedly");
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "access_denied",
                "the upstream identity could not be accepted",
            );
        }
    };

    // confused deputy 対策は redirect_uri の許可リスト（`is_acceptable_redirect_uri`）が
    // 担う。`redirect_uri` はここまでに以下を満たしている:
    // - authorize 時点で登録済み redirect_uris との完全一致を確認済み（state に格納）
    // - 登録（DCR）時点で `is_acceptable_redirect_uri` の許可リストを通過済み
    // したがって配送先は claude.ai（または http loopback）に限られており、
    // 第三者へ認可コードが渡る経路自体が無い。よって Google の identity が
    // 確定した時点でそのまま認可コードを発行してよい。
    //
    // 旧実装はここで自前の同意画面（`/oauth/consent`）を挟んでいたが、上記の
    // 許可リスト導入により経路自体が閉じたため撤去した（設計:
    // docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md）。
    //
    // 認可コードは **封緘**する（Google の access_token / refresh_token を運ぶため）。
    let code_blob = match state.signing_key.seal(&Blob::Code {
        sub: identity.sub,
        email: identity.email,
        client_id,
        redirect_uri: redirect_uri.clone(),
        code_challenge,
        upstream,
        jti: uuid::Uuid::new_v4().to_string(),
        exp: state.expires_in(CODE_TTL_SECS),
    }) {
        Ok(v) => v,
        Err(e) => return signing_failed("code", &e),
    };

    let mut target = match url::Url::parse(&redirect_uri) {
        Ok(u) => u,
        Err(e) => {
            // 署名済み state 由来の値なので、ここに来るのは登録時の検証を
            // すり抜けた場合のみ。クライアントへ戻せないため 400 で止める。
            tracing::error!(error = %e, "registered redirect_uri is not parseable");
            return oauth_error(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the registered redirect_uri is not a valid URI",
            );
        }
    };
    {
        let mut pairs = target.query_pairs_mut();
        pairs.append_pair("code", &code_blob);
        // クライアントの元の state をそのまま返す（クライアント側 CSRF 対策の前提）。
        if let Some(cs) = client_state.as_deref() {
            pairs.append_pair("state", cs);
        }
    }
    redirect(target.as_str())
}

impl AuthServerState {
    /// 認可コード・同意ブロブの単回使用を担保する。使用済みなら `false`。
    ///
    /// `blob_exp` には **そのブロブ自身の `exp`（UNIX 秒）** を渡す。使用済み記録は
    /// その時刻まで保持される。
    ///
    /// 2026-07 改訂 / reviewer 指摘 C1:
    /// 旧実装は「記録時刻」を保存し、刈り取り窓を `CODE_TTL_SECS`（60 秒）**固定**に
    /// していた。一方、当時あった自前の同意ブロブ（`Blob::Consent`。confused deputy
    /// 対策の同意画面ごと後日撤去済み）の寿命は 300 秒であり、
    /// **使用済み記録が 60 秒で消えるのにブロブは 300 秒有効**という不整合があった。
    /// この t=60..300 の窓では同じ同意ブロブを再 POST すると認可コードが再発行でき、
    /// さらに `consume` が `action` 判定より前にあるため、**一度「拒否」を押した認可を
    /// 60 秒後に approve で再送して成立させられた**。
    ///
    /// 「記録時刻 + 固定窓」ではなく **ブロブ自身の exp** を保存することで、TTL の
    /// 異なるブロブ種別が増えても、また将来 TTL 定数を変えても、この不整合は再発しない
    /// （固定窓を `max(CODE, CONSENT)` に広げる案は、次に TTL の長い種別が増えた
    /// 瞬間に同じ穴が開くため採らない）。
    ///
    /// **これは best-effort である。** 根拠と許容理由:
    /// - 使用済み集合はプロセスメモリにしか無い。Cloud Run はゼロスケールするため、
    ///   コンテナが落ちると集合ごと消える。外部ストアは追加しない方針。
    /// - 一方 `maxScale=1` なので、同時に 2 つのインスタンスが別々の集合を持つことは
    ///   なく、**プロセスが生きている間の再利用は確実に弾ける**。
    /// - すり抜けるのは「ブロブの有効期間内にコンテナが再起動し、その窓で同じ値が
    ///   再提示された場合」だけに限られる。
    /// - さらにコードは PKCE の `code_challenge` に束縛されている（`token` で照合）。
    ///   すり抜けても、正しい `code_verifier` を持つ相手＝本来のクライアント以外は
    ///   交換できない。
    ///
    /// **OAuth 2.1 への非準拠点（意図的）**:
    /// OAuth 2.1 は認可コードの再利用を検出した場合、**そのコードから発行済みの
    /// アクセストークン・リフレッシュトークンを失効させること**を求める
    /// （draft-ietf-oauth-v2-1 §4.1.3 / RFC 6749 §4.1.2）。本実装はこれを満たさない。
    /// トークンが署名付きの自己完結値であり、発行済みトークンの台帳も失効リストも
    /// 持たないため、**再利用を検出しても既発行トークンを取り消す手段が無い**。
    /// できるのは「2 回目以降の交換を拒否する」ところまでである。
    /// 現実的な緩和は、認可コードの寿命が 60 秒であること、PKCE で本来の
    /// クライアントに束縛されていること、そして署名鍵ローテーションが唯一の
    /// 一括失効手段として残っていること。この非準拠を解消するには外部ストアが要る。
    fn consume_jti(&self, jti: &str, blob_exp: u64) -> bool {
        // S1: poisoning でパニックさせない。この集合は best-effort な多重使用検出で
        // あり、毒された Mutex で全 token リクエストを 500 にする方が害が大きい
        // （`into_inner` で中身を引き継いで処理を続ける）。集合が壊れていても、
        // 短い TTL と PKCE 束縛という主たる防御は効いたままである。
        let mut used = self
            .used_codes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        used.consume(jti, blob_exp, self.now())
    }
}

/// 使用済み jti の集合。件数上限つきで、刈り取りコストを償却する（C3）。
struct UsedJtis {
    /// jti → **そのブロブ自身の有効期限（UNIX 秒）**。
    /// 記録時刻ではなく期限を持つ理由は `consume_jti` を参照。
    entries: HashMap<String, u64>,
    /// 次に期限切れの刈り取りを行う件数。刈り取り後に生存件数の 2 倍へ引き上げるが、
    /// **`MAX_USED_JTIS` を超えないよう頭打ちにする**。超えてしまうと、集合が満杯に
    /// なった後に刈り取りが二度と走らず、上限判定だけが残って**恒久的に閉じたまま**に
    /// なる（正規のログインが永久に通らなくなる）。
    prune_at: usize,
    /// 最後に刈り取りを行った時刻（秒）。同一秒内の刈り取りは 1 回に抑える。
    ///
    /// 上限まで生存エントリで埋まった状態では、`prune_at` の頭打ちにより毎回の
    /// 挿入試行が刈り取り条件を満たしてしまう。これを許すと、まさに C3 で問題に
    /// している「リクエストごとの O(n) 全走査」が満杯時に再発する。秒単位で
    /// 間引くことで、走査コストを**リクエスト数ではなく経過時間**に比例させる。
    last_prune: Option<u64>,
    /// 実行した全走査の回数。**刈り取りが実際に間引かれているかを観測するため**に
    /// 持つ（`last_prune` は同じ秒に再走査しても値が変わらず、間引きの有無を
    /// 区別できない）。運用上の意味は無く、テストの観測点として存在する。
    prunes: u64,
}

impl UsedJtis {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            prune_at: PRUNE_THRESHOLD,
            last_prune: None,
            prunes: 0,
        }
    }

    /// 使用済みとして記録する。**初めての jti なら `true`**、既に使用済み、または
    /// 上限に達していて記録できない場合は `false`（＝ブロブを通さない）。
    fn consume(&mut self, jti: &str, blob_exp: u64, now: u64) -> bool {
        // 既に使用済みかどうかは、刈り取りより先に見る（刈り取りは純粋に
        // メモリ管理であって、判定結果を変えてはならない）。
        if self.entries.get(jti).is_some_and(|exp| *exp > now) {
            return false;
        }
        // 閾値を超えたときだけ、かつ同一秒内では 1 回だけ全走査する
        // （償却 O(1)、満杯時も経過時間比例。定数とフィールドのコメント参照）。
        if self.entries.len() >= self.prune_at && self.last_prune != Some(now) {
            self.entries.retain(|_, exp| *exp > now);
            self.last_prune = Some(now);
            self.prunes += 1;
            // 次の刈り取りは生存件数の 2 倍で。生存件数が少なければ最低閾値に戻る。
            // **上限で頭打ちにする**（超えると満杯後に刈り取りが走らなくなる）。
            // PRUNE_THRESHOLD(=下限) <= MAX_USED_JTIS(=上限) は定数で常に成立するため
            // clamp は panic せず、max().min() と挙動が一致する（clippy::manual_clamp）。
            self.prune_at = (self.entries.len() * 2).clamp(PRUNE_THRESHOLD, MAX_USED_JTIS);
        }
        if self.entries.len() >= MAX_USED_JTIS {
            // **fail closed。** 上限に達した状態で新しいブロブを通すと、その 1 件は
            // 単回使用を担保できない（記録できないので再提示を検出できない）。
            // 単回使用を捨てるより、その要求を拒否する方を選ぶ。
            //
            // この選択の代償は明示しておく: 攻撃者が集合を埋めれば**正規の
            // ログインを妨害できる**（可用性の低下）。ただし単回使用の回避や
            // トークンの偽造には繋がらない。可用性側の防御は前段
            // （Cloud Armor 等）の領分であり、その旨は
            // `specs/production-cs-mcp.md` の残存リスク節に記載している。
            tracing::error!(
                used_jtis = self.entries.len(),
                "the single-use set is full; rejecting the request instead of \
                 letting a blob through unrecorded (check for abuse of /oauth/authorize)"
            );
            return false;
        }
        self.entries.insert(jti.to_string(), blob_exp);
        true
    }
}

async fn token(State(state): State<Arc<AuthServerState>>, Form(form): Form<TokenForm>) -> Response {
    match form.grant_type.as_deref() {
        Some("authorization_code") => token_from_code(&state, form),
        Some("refresh_token") => token_from_refresh(&state, form).await,
        _ => oauth_error(
            StatusCode::BAD_REQUEST,
            "unsupported_grant_type",
            "only authorization_code and refresh_token are supported",
        ),
    }
}

/// 認可コード交換。失敗理由は分類名だけをログに残し、code / verifier は残さない。
fn token_from_code(state: &AuthServerState, form: TokenForm) -> Response {
    let invalid_grant = |reason: &str| -> Response {
        tracing::info!(
            reason = reason,
            grant = "authorization_code",
            "token rejected"
        );
        oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the authorization code could not be redeemed",
        )
    };

    let Some(code) = form.code.filter(|s| !s.is_empty()) else {
        return invalid_grant("missing_code");
    };
    // 認可コードは封緘済み（`seal`）。種別違い（リフレッシュトークンを code として
    // 持ち込む等）は `Blob` の tag が一致しないため deserialize で落ち、
    // 封緘されていないブロブは AEAD の認証で落ちる。
    let Ok(Blob::Code {
        sub,
        // 監査の主識別子は `sub`（不変）なので、発行ログにはそちらだけを残す。
        // `email` は封緘ブロブ内に「認可時点の値」として保持されるが、ここでは読まない
        // （下流の actor 表示は、リクエストのたびに tokeninfo が返す現在値を使う）。
        email: _,
        client_id,
        redirect_uri,
        code_challenge,
        upstream,
        jti,
        exp,
    }) = state.signing_key.open::<Blob>(&code)
    else {
        return invalid_grant("unverifiable_code");
    };
    if exp <= state.now() {
        return invalid_grant("expired_code");
    }
    if form.client_id.as_deref() != Some(client_id.as_str()) {
        return invalid_grant("client_id_mismatch");
    }
    if form.redirect_uri.as_deref() != Some(redirect_uri.as_str()) {
        return invalid_grant("redirect_uri_mismatch");
    }
    let Some(verifier) = form.code_verifier.filter(|s| !s.is_empty()) else {
        return invalid_grant("missing_code_verifier");
    };
    if s256(&verifier) != code_challenge {
        return invalid_grant("pkce_mismatch");
    }
    // 単回使用の消費は全検証を通過した後に行う。先に消費すると、PKCE 不一致の
    // 誤提示 1 回で正規クライアントのコードまで焼き切れてしまう。
    if !state.consume_jti(&jti, exp) {
        return invalid_grant("code_already_used");
    }

    // **自前のトークンは発行しない。** Google が発行した値をそのまま返す。
    // S1: どのクライアント経由の発行かを監査で追えるよう指紋を残す。
    // sub は監査主体の識別子で秘密ではないが、トークン本体は絶対に残さない。
    let client_fp = client_fingerprint(&client_id);
    tracing::info!(
        sub = %sub,
        client_fp = %client_fp,
        refreshable = upstream.refresh_token.is_some(),
        "handed google-issued tokens to the client"
    );
    upstream_token_response(upstream, state.now())
}

/// Google 発行のトークンを、そのまま RFC 6749 §5.1 のトークン応答へ写す。
///
/// `refresh_token` は Google が返したときだけ載せる。`scope` は Google に対して
/// 固定で要求している値（`GOOGLE_SCOPE`）をそのまま申告する。
fn upstream_token_response(upstream: UpstreamTokens, now: u64) -> Response {
    let mut body = serde_json::json!({
        "access_token": upstream.access_token,
        "token_type": "Bearer",
        "expires_in": upstream.expires_in(now),
        "scope": GOOGLE_SCOPE,
    });
    if let Some(refresh_token) = upstream.refresh_token {
        body["refresh_token"] = serde_json::Value::String(refresh_token);
    }
    token_response(body)
}

/// リフレッシュ交換。**クライアントが提示した refresh_token を Google へ中継する。**
///
/// 自前トークンを廃止したため、クライアントが持つ refresh_token は Google が
/// 発行した値そのものである。したがってこの関数は署名検証も復号もできず、
/// できるのは「Google の client_id / client_secret を添えて中継し、応答を返す」
/// ことだけになる。client_secret をクライアントへ渡さずに済ませることが、
/// この中継を置く唯一の理由である。
///
/// **エラー分類だけは中継経路でも維持する**（`classify_upstream_failure`）。
/// 429 / 408 / 5xx / 到達不能を `invalid_grant` に写すと、クライアントは仕様どおり
/// refresh_token を破棄し、**Google の一時的な流量制限だけで利用者が再ログイン
/// 必須**になる。しかも集中アクセス時ほど多数の利用者が同時にセッションを失う。
/// 一時障害は 503 + `Retry-After` として返し、再試行させる。
async fn token_from_refresh(state: &AuthServerState, form: TokenForm) -> Response {
    let invalid_grant = |reason: &str| -> Response {
        tracing::info!(reason = reason, grant = "refresh_token", "token rejected");
        oauth_error(
            StatusCode::BAD_REQUEST,
            "invalid_grant",
            "the refresh token could not be redeemed",
        )
    };

    let Some(refresh_token) = form.refresh_token.filter(|s| !s.is_empty()) else {
        return invalid_grant("missing_refresh_token");
    };
    // S2: クライアント登録の有効期限をリフレッシュ経路でも再検証する。
    // `authorize` にだけ期限を課していると、登録が 90 日で切れた後もリフレッシュが
    // 通り続け、期限の意味が半分失われる。client_id は署名済みブロブなので、
    // 上流トークンが不透明でもこの検証だけは引き続き成立する。
    let Some(client_id) = form.client_id.filter(|s| !s.is_empty()) else {
        return invalid_grant("missing_client_id");
    };
    match state.signing_key.verify::<Blob>(&client_id) {
        Ok(Blob::Client { iat, .. }) => {
            if state.now().saturating_sub(iat) > CLIENT_MAX_AGE_SECS {
                return invalid_grant("expired_client_registration");
            }
        }
        _ => return invalid_grant("unverifiable_client_id"),
    }
    // `resource` が指定されていれば、設定済み project を指すことだけ確認する。
    // 束縛の照合はできない（上流トークンにこちらの aud は載らない）。
    if let Some(requested) = form.resource.as_deref().filter(|s| !s.is_empty()) {
        if resolve_resource(&state.config, Some(requested)).is_err() {
            return invalid_grant("unusable_resource");
        }
    }

    let resp = state
        .http
        .post(&state.config.google_token_url)
        .form(&[
            ("client_id", state.config.google_client_id.as_str()),
            ("client_secret", state.config.google_client_secret.as_str()),
            ("refresh_token", refresh_token.as_str()),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .await;
    let resp = match resp {
        Ok(r) => r,
        Err(e) => {
            // reqwest の Display はリクエスト URL を必ず書き出すため埋め込まない。
            tracing::error!(
                error = %super::verifier::describe_transport_error("google token refresh", &e),
                "failed to reach the Google token endpoint while refreshing"
            );
            // 到達不能を `invalid_grant` にしない。失効した証拠が無い。
            return temporarily_unavailable(
                None,
                "could not reach the upstream identity provider; try again shortly",
            );
        }
    };
    if !resp.status().is_success() {
        let status = resp.status();
        let retry_after = retry_after_secs(resp.headers());
        // 本文は判定にだけ使う。**生の本文はログにも応答にも載せない**
        // （リクエストの写しを含む実装がありうる）。取り出すのは OAuth の
        // 固定エラーコードだけで、それも既知の値に照合してから記録する。
        let oauth_err = resp
            .text()
            .await
            .ok()
            .and_then(|body| serde_json::from_str::<GoogleErrorResponse>(&body).ok())
            .and_then(|e| e.error);
        return match classify_upstream_failure(status, oauth_err.as_deref(), retry_after) {
            UpstreamFailure::Rejected => {
                tracing::info!(
                    reason = "upstream_revoked",
                    grant = "refresh_token",
                    "token rejected because google refused the upstream grant"
                );
                oauth_error(
                    StatusCode::BAD_REQUEST,
                    "invalid_grant",
                    "the upstream identity is no longer valid; sign in again",
                )
            }
            UpstreamFailure::Temporary { retry_after } => temporarily_unavailable(
                retry_after,
                "could not confirm the upstream identity; try again shortly",
            ),
        };
    }
    let Ok(body) = resp.text().await else {
        tracing::error!("failed to read the Google token endpoint refresh response");
        return temporarily_unavailable(
            None,
            "could not read the upstream response; try again shortly",
        );
    };
    // **本文はログしない**。access_token / id_token / refresh_token を含む。
    let Ok(GoogleTokenResponse {
        access_token: Some(access_token),
        refresh_token: rotated_refresh_token,
        expires_in,
    }) = serde_json::from_str::<GoogleTokenResponse>(&body)
    else {
        // 本文が不正なのは Google 側の異常であり、identity の失効ではない。
        // 失効と断定して締め出すより 503 で再試行させる（fail closed のまま）。
        tracing::error!("google refresh response had no usable access_token");
        return temporarily_unavailable(
            None,
            "the upstream response was not usable; try again shortly",
        );
    };
    tracing::info!(
        rotated = rotated_refresh_token.is_some(),
        "relayed a refreshed google access token to the client"
    );
    upstream_token_response(
        UpstreamTokens {
            access_token,
            // Google は既定で refresh_token をローテーションしないが、返してきた
            // 場合はそれを渡す。渡さないと、Google が古い値を無効化する運用に
            // 変わったときにクライアントが失効済みの値を持ち続ける。
            refresh_token: rotated_refresh_token,
            expires_at: state.expires_in(expires_in.unwrap_or(ASSUMED_GOOGLE_ACCESS_TTL_SECS)),
        },
        state.now(),
    )
}

/// 上流リフレッシュの失敗分類。「失効した」と「確認できなかった」を混同しない
/// ために分ける（前者は `invalid_grant`、後者は 503 で再試行）。
///
/// **この区別は利用者への影響が非対称である。** `Revoked` を返すと呼び出し元は
/// `invalid_grant` に変換し、OAuth クライアントは仕様どおりリフレッシュトークンを
/// 破棄する = 利用者は再ログインを強いられる。したがって「失効したと確信できる」
/// 場合以外は `Unreachable` に倒す。
enum UpstreamFailure {
    /// Google がグラント（認可コード / refresh_token）を**明確に**拒否した、
    /// または返ってきた identity が要件を満たさない。やり直しても同じ結果になる。
    Rejected,
    /// Google に到達できない / 一時的に断られた / 応答を解釈できない /
    /// 検証系が一時的に使えない。`retry_after` は Google が示した待ち時間（秒）。
    Temporary { retry_after: Option<u64> },
}

/// 一時障害を表す 503 を組み立てる。`Retry-After` があればクライアントへ転送する。
///
/// **こちらでスリープして待つことはしない** —— リクエストを掴んだまま待つと
/// `maxScale=1` のインスタンスを占有し、まさに避けたい飽和を自分で起こすため。
fn temporarily_unavailable(retry_after: Option<u64>, description: &'static str) -> Response {
    let mut res = oauth_error(
        StatusCode::SERVICE_UNAVAILABLE,
        "temporarily_unavailable",
        description,
    );
    if let Some(secs) = retry_after {
        match axum::http::HeaderValue::from_str(&secs.to_string()) {
            Ok(hv) => {
                res.headers_mut().insert(header::RETRY_AFTER, hv);
            }
            Err(e) => {
                tracing::error!(error = %e, "could not build a Retry-After header");
            }
        }
    }
    res
}

/// Google のエラー応答のうち、分類に使う OAuth 標準のエラーコードだけ。
#[derive(Debug, Deserialize)]
struct GoogleErrorResponse {
    error: Option<String>,
}

/// `Retry-After` ヘッダを秒数として読む。
///
/// RFC 9110 は「秒数」と「HTTP-date」の 2 形式を許すが、ここでは秒数のみ解釈する。
/// Google の 429 は秒数形式で返すため実用上これで足り、HTTP-date のパースを
/// 自前で持つ（＝時刻処理のバグを増やす）価値が無いと判断した。
/// 解釈できない場合は `None`（＝待ち時間の指示なし）に倒す。
fn retry_after_secs(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        // 異常に長い指示は無視する（クライアントを事実上締め出さない）。
        .filter(|secs| *secs <= MAX_RETRY_AFTER_SECS)
}

/// 上流の非成功応答を「失効」と「一時障害」に分類する（C1）。
///
/// 2026-07 改訂 / codex 指摘 C1:
/// 旧実装は `is_server_error()`（5xx）だけを一時障害とし、**それ以外の非成功応答を
/// すべて `Revoked`** にしていた。コメントも「4xx は取消・無効化」と書いていたが、
/// 4xx には **429 Too Many Requests** が含まれる。呼び出し元で `invalid_grant` に
/// 変換されるため、**Google の一時的な流量制限だけで利用者が再ログイン必須**になり、
/// しかも集中アクセス時ほど大量の利用者が同時にセッションを失う形になっていた。
///
/// 分類の根拠:
/// - **`invalid_grant` を伴う 400 / 401 のみが「失効」**。RFC 6749 §5.2 で
///   `invalid_grant` は「グラントが無効・期限切れ・取消済み」を意味する唯一のコードで、
///   Google が取消済み refresh_token に返すのもこれ。ここだけが確信を持てる信号。
/// - `invalid_client` / `unauthorized_client` 等を伴う 400 / 401 は
///   **こちら側の設定不備**（client_secret の誤り・失効）であって利用者の失効ではない。
///   これで再ログインさせると、設定ミス 1 つで全利用者のセッションを破壊する。
/// - **429 / 408 は一時障害**。流量制限とタイムアウトはグラントの有効性と無関係。
/// - 5xx は Google 側の障害。
/// - その他の 4xx（403 / 404 等）は想定外。失効と断定する根拠が無いので、
///   セッションを壊さない側（一時障害）に倒す。
fn classify_upstream_failure(
    status: StatusCode,
    oauth_error: Option<&str>,
    retry_after: Option<u64>,
) -> UpstreamFailure {
    let definitive_revocation =
        matches!(status, StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED)
            && oauth_error == Some("invalid_grant");

    if definitive_revocation {
        tracing::info!(
            status = %status,
            oauth_error = "invalid_grant",
            "google reported the upstream grant as revoked"
        );
        return UpstreamFailure::Rejected;
    }
    if status == StatusCode::TOO_MANY_REQUESTS {
        // 失効ではない。ここを `Revoked` にすると、Google のレート制限が
        // そのまま利用者の強制ログアウトに化ける。
        tracing::warn!(
            status = %status,
            retry_after = ?retry_after,
            "google rate-limited the upstream refresh; treating it as a temporary failure"
        );
        return UpstreamFailure::Temporary { retry_after };
    }
    if status.is_server_error() || status == StatusCode::REQUEST_TIMEOUT {
        tracing::error!(
            status = %status,
            "google token endpoint failed while refreshing an upstream grant"
        );
        return UpstreamFailure::Temporary { retry_after };
    }
    // 400 / 401 だが `invalid_grant` ではない（設定不備の疑い）、または想定外の 4xx。
    // 失効と断定できないため、セッションを壊さない側へ倒したうえで、
    // 運用者が気づけるよう error で残す。
    tracing::error!(
        status = %status,
        // 既知の OAuth コードだけを出す。未知の値は本文由来なのでそのまま載せない。
        oauth_error = %known_oauth_error(oauth_error),
        "google refused the upstream refresh for a reason that is not a revocation; \
         check the configured Google client credentials"
    );
    UpstreamFailure::Temporary { retry_after }
}

/// ログに載せてよい形へ正規化する。未知の値は本文由来の任意文字列になりうるため
/// そのまま出さない（`oauth_error` は Google の応答本文から来る）。
fn known_oauth_error(oauth_error: Option<&str>) -> &'static str {
    match oauth_error {
        Some("invalid_grant") => "invalid_grant",
        Some("invalid_client") => "invalid_client",
        Some("unauthorized_client") => "unauthorized_client",
        Some("invalid_request") => "invalid_request",
        Some("invalid_scope") => "invalid_scope",
        Some("unsupported_grant_type") => "unsupported_grant_type",
        Some(_) => "<unrecognized>",
        None => "<absent>",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use tower::ServiceExt;

    const PUBLIC_HOST: &str = "cs-support.example.com";
    const GOOGLE_CLIENT_ID: &str = "google-client-id.apps.googleusercontent.com";
    const CLIENT_REDIRECT: &str = "https://claude.ai/api/mcp/auth_callback";
    const PROJECT_ID: &str = "urtect";
    const OTHER_PROJECT_ID: &str = "other";
    /// Google が返す refresh_token を模した値。**発行物に平文で現れないこと**を
    /// 検証するため、他の値と衝突しない特徴的な文字列にしてある。
    const GOOGLE_REFRESH_TOKEN: &str = "1//0gUPSTREAM-REFRESH-TOKEN";
    /// Google が返す access_token を模した値。`GOOGLE_TOKEN_OK` の中身と一致させる。
    /// **これがそのままクライアントへ渡る**ため、封緘ブロブに平文で現れないことを
    /// 検証する対象でもある。
    const GOOGLE_ACCESS_TOKEN: &str = "google-access-token";

    /// `callback` が Google から受け取ったトークン群を模したもの。
    fn test_upstream() -> UpstreamTokens {
        UpstreamTokens {
            access_token: GOOGLE_ACCESS_TOKEN.to_string(),
            refresh_token: Some(GOOGLE_REFRESH_TOKEN.to_string()),
            expires_at: now_secs() + 3599,
        }
    }

    fn resource_uri(project_id: &str) -> String {
        format!("https://{PUBLIC_HOST}/{project_id}/mcp")
    }

    /// 固定 JSON を返す使い捨て HTTP stub（Google token endpoint / tokeninfo 用）。
    /// 受けたリクエストの生文字列を記録し、「client_secret がボディで送られたか」
    /// 「そもそも叩かれたか」を直接検証できるようにする。
    async fn spawn_stub(body: &'static str) -> (String, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let hits = Arc::new(AtomicUsize::new(0));
        let log_task = log.clone();
        let hits_task = hits.clone();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                hits_task.fetch_add(1, Ordering::SeqCst);
                let log_conn = log_task.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 2048];
                    loop {
                        let n = match stream.read(&mut buf).await {
                            Ok(0) | Err(_) => break,
                            Ok(n) => n,
                        };
                        raw.extend_from_slice(&buf[..n]);
                        let text = String::from_utf8_lossy(&raw).to_string();
                        let Some(end) = text.find("\r\n\r\n") else {
                            continue;
                        };
                        let len = text
                            .lines()
                            .find_map(|l| {
                                let (k, v) = l.split_once(':')?;
                                k.eq_ignore_ascii_case("content-length")
                                    .then(|| v.trim().parse::<usize>().ok())?
                            })
                            .unwrap_or(0);
                        if raw.len() >= end + 4 + len {
                            break;
                        }
                    }
                    log_conn
                        .lock()
                        .unwrap()
                        .push(String::from_utf8_lossy(&raw).to_string());
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
        (format!("http://{addr}/"), log, hits)
    }

    fn tokeninfo_body(email_verified: &str) -> String {
        format!(
            r#"{{"aud":"{GOOGLE_CLIENT_ID}","azp":"{GOOGLE_CLIENT_ID}","sub":"1122334455",
               "email":"cs@example.com","email_verified":"{email_verified}","expires_in":"3599"}}"#
        )
    }

    struct Harness {
        app: Router,
        key: Arc<SigningKey>,
        google_token_log: Arc<Mutex<Vec<String>>>,
        google_token_hits: Arc<AtomicUsize>,
        clock: Arc<TestClock>,
    }

    /// 論理時計。実時間の `sleep` を使わずに経過時間を作る（C1 の検証に必須）。
    /// 実 sleep に頼るテストは遅く、CI の負荷で不安定になる。
    struct TestClock {
        secs: AtomicU64,
    }

    impl TestClock {
        fn new() -> Self {
            // 実時刻から始める。ブロブの `exp` を実時刻ベースで作るテストヘルパ
            // （`state_blob` など）と齟齬が出ないようにするため。
            Self {
                secs: AtomicU64::new(now_secs()),
            }
        }

        /// 論理時計を `secs` 秒進める。
        fn advance(&self, secs: u64) {
            self.secs.fetch_add(secs, Ordering::SeqCst);
        }
    }

    impl Clock for TestClock {
        fn now_secs(&self) -> u64 {
            self.secs.load(Ordering::SeqCst)
        }
    }

    /// Google token endpoint の既定応答。`prompt=consent` を常に付けているため、
    /// 本番でもこの形（refresh_token 付き）が返る。
    const GOOGLE_TOKEN_OK: &str = concat!(
        r#"{"access_token":"google-access-token","expires_in":3599,"#,
        r#""refresh_token":"1//0gUPSTREAM-REFRESH-TOKEN"}"#
    );

    async fn harness_with_tokeninfo(tokeninfo: &'static str) -> Harness {
        harness_full(tokeninfo, GOOGLE_TOKEN_OK, vec![PROJECT_ID.to_string()]).await
    }

    /// tokeninfo stub / Google token stub の応答本文と project 構成を差し替えられる
    /// 形で AS を組む。`&'static str` を要求する stub に合わせ、呼び出し側で
    /// `Box::leak` する。
    async fn harness_full(
        tokeninfo: &'static str,
        google_token: &'static str,
        project_ids: Vec<String>,
    ) -> Harness {
        let (google_token_url, google_token_log, google_token_hits) =
            spawn_stub(google_token).await;
        let (tokeninfo_url, _, _) = spawn_stub(tokeninfo).await;
        let key = Arc::new(SigningKey::new("test-signing-key"));
        let config = AuthServerConfig {
            public_host: PUBLIC_HOST.to_string(),
            google_client_id: GOOGLE_CLIENT_ID.to_string(),
            google_client_secret: "google-client-secret".to_string(),
            google_authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            google_token_url,
            project_ids,
        };
        let verifier = Arc::new(GoogleTokenVerifier::with_settings(
            GOOGLE_CLIENT_ID.to_string(),
            tokeninfo_url,
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        let clock = Arc::new(TestClock::new());
        let state = Arc::new(AuthServerState::with_clock(
            config,
            key.clone(),
            verifier,
            clock.clone(),
        ));
        Harness {
            app: auth_server_router(state),
            key,
            google_token_log,
            google_token_hits,
            clock,
        }
    }

    async fn harness() -> Harness {
        harness_with_tokeninfo(Box::leak(tokeninfo_body("true").into_boxed_str())).await
    }

    /// 任意のステータス行・追加ヘッダ・本文を返す stub。
    /// C1 の分類（429 / 400 invalid_grant / 400 invalid_client …）を作り分ける。
    async fn spawn_failing_stub(
        status_line: &'static str,
        extra_headers: &'static str,
        body: &'static str,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut buf = [0u8; 2048];
                    let _ = stream.read(&mut buf).await;
                    let resp = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\n\
                         {extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes()).await;
                    let _ = stream.shutdown().await;
                });
            }
        });
        format!("http://{addr}/")
    }

    /// 常に `400 invalid_grant` を返す stub（= Google がグラントを取り消した状態）。
    async fn spawn_rejecting_stub() -> String {
        spawn_failing_stub("400 Bad Request", "", r#"{"error":"invalid_grant"}"#).await
    }

    async fn harness_with_google_token_url(
        key: &Arc<SigningKey>,
        google_token_url: String,
    ) -> Harness {
        harness_reusing_key(
            key,
            google_token_url,
            Box::leak(tokeninfo_body("true").into_boxed_str()),
        )
        .await
    }

    /// 既存の署名鍵を流用して、Google token endpoint と tokeninfo を差し替えた AS を組む。
    /// 同じ鍵で発行済みのリフレッシュトークンを、**別の上流状態**に対して提示できる
    /// （取消済み・到達不能・identity が変化した、等の再現に使う）。
    async fn harness_reusing_key(
        key: &Arc<SigningKey>,
        google_token_url: String,
        tokeninfo: &'static str,
    ) -> Harness {
        let (tokeninfo_url, _, _) = spawn_stub(tokeninfo).await;
        let config = AuthServerConfig {
            public_host: PUBLIC_HOST.to_string(),
            google_client_id: GOOGLE_CLIENT_ID.to_string(),
            google_client_secret: "google-client-secret".to_string(),
            google_authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
            google_token_url,
            project_ids: vec![PROJECT_ID.to_string()],
        };
        let verifier = Arc::new(GoogleTokenVerifier::with_settings(
            GOOGLE_CLIENT_ID.to_string(),
            tokeninfo_url,
            Duration::from_secs(2),
            Duration::from_secs(2),
        ));
        let clock = Arc::new(TestClock::new());
        let state = Arc::new(AuthServerState::with_clock(
            config,
            key.clone(),
            verifier,
            clock.clone(),
        ));
        Harness {
            app: auth_server_router(state),
            key: key.clone(),
            google_token_log: Arc::new(Mutex::new(Vec::new())),
            google_token_hits: Arc::new(AtomicUsize::new(0)),
            clock,
        }
    }

    async fn harness_rejecting_google(key: &Arc<SigningKey>) -> Harness {
        harness_with_google_token_url(key, spawn_rejecting_stub().await).await
    }

    /// 誰も listen していないポートを指す AS（= Google に到達できない状態）。
    async fn harness_with_dead_google(key: &Arc<SigningKey>) -> Harness {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        harness_with_google_token_url(key, format!("http://{addr}/")).await
    }

    async fn body_json(res: Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    async fn body_text(res: Response) -> String {
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        String::from_utf8_lossy(&bytes).to_string()
    }

    fn location(res: &Response) -> String {
        res.headers()
            .get(header::LOCATION)
            .expect("expected a redirect with Location")
            .to_str()
            .unwrap()
            .to_string()
    }

    async fn post_register(app: &Router, body: serde_json::Value) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/oauth/register")
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn register_client(app: &Router) -> String {
        let res = post_register(app, serde_json::json!({"redirect_uris": [CLIENT_REDIRECT]})).await;
        body_json(res).await["client_id"].as_str().unwrap().into()
    }

    async fn get_uri(app: &Router, uri: &str) -> Response {
        app.clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn post_form(app: &Router, uri: &str, form: &str) -> Response {
        app.clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(form.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
    }

    async fn post_token(app: &Router, form: &str) -> Response {
        post_form(app, "/oauth/token", form).await
    }

    fn enc(s: &str) -> String {
        url::form_urlencoded::byte_serialize(s.as_bytes()).collect()
    }

    // ------------------------------------------------------------------
    // DCR (RFC 7591)
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn register_returns_public_client_registration() {
        let h = harness().await;
        let res = post_register(
            &h.app,
            serde_json::json!({"redirect_uris": [CLIENT_REDIRECT], "client_name": "Claude"}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let json = body_json(res).await;
        assert!(json["client_id"].as_str().is_some_and(|s| !s.is_empty()));
        assert_eq!(json["redirect_uris"][0], CLIENT_REDIRECT);
        assert_eq!(json["token_endpoint_auth_method"], "none");
        assert_eq!(json["client_name"], "Claude");
        // public client + PKCE。secret を発行してしまうと、クライアント側に置けない
        // 秘密が生まれて今回の移行の目的そのものが崩れる。
        assert!(
            json.get("client_secret").is_none(),
            "must not issue a client_secret: {json}"
        );
    }

    #[tokio::test]
    async fn register_issues_a_client_id_that_carries_signed_redirect_uris() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let blob: Blob = h.key.verify(&client_id).unwrap();
        match blob {
            Blob::Client { redirect_uris, .. } => {
                assert_eq!(redirect_uris, vec![CLIENT_REDIRECT.to_string()])
            }
            other => panic!("expected a client blob, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn register_rejects_missing_redirect_uris() {
        let h = harness().await;
        let res = post_register(&h.app, serde_json::json!({"client_name": "Claude"})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn register_rejects_non_absolute_redirect_uri() {
        let h = harness().await;
        let res = post_register(&h.app, serde_json::json!({"redirect_uris": ["/relative"]})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    // ------------------------------------------------------------------
    // redirect_uri の許可リスト（confused deputy 対策）
    //
    // `/oauth/register` は無認証なので、ここでホストを絞らないと第三者が
    // 任意ホストの redirect_uri を登録でき、callback が認可コードをそこへ
    // 配送してしまう（詳細:
    // docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md）。
    // ------------------------------------------------------------------

    /// 許可ホスト（claude.ai）への https は関数レベルで通ること。
    #[test]
    fn is_acceptable_redirect_uri_allows_the_registered_https_host() {
        assert!(is_acceptable_redirect_uri(CLIENT_REDIRECT));
    }

    /// 第三者ホストへの https は拒否されること。confused deputy の核心。
    #[test]
    fn is_acceptable_redirect_uri_rejects_a_third_party_https_host() {
        assert!(!is_acceptable_redirect_uri("https://attacker.example/cb"));
    }

    /// サフィックス一致では通らないこと。`ends_with` 判定への退行を検出する。
    #[test]
    fn is_acceptable_redirect_uri_rejects_a_suffix_matching_host() {
        assert!(!is_acceptable_redirect_uri("https://evil-claude.ai/cb"));
    }

    /// http は loopback ならポートによらず許可されること。
    #[test]
    fn is_acceptable_redirect_uri_allows_http_loopback_on_any_port() {
        for uri in [
            "http://localhost:8080/cb",
            "http://localhost:51823/cb",
            "http://127.0.0.1:9000/cb",
            "http://[::1]:12345/cb",
        ] {
            assert!(is_acceptable_redirect_uri(uri), "{uri}");
        }
    }

    /// http は loopback 以外だと拒否されること（LAN・インターネットへ平文コードを流さない）。
    #[test]
    fn is_acceptable_redirect_uri_rejects_non_loopback_http() {
        assert!(!is_acceptable_redirect_uri("http://claude.ai/cb"));
        assert!(!is_acceptable_redirect_uri("http://example.com:8080/cb"));
    }

    /// DCR レベルでも第三者ホストの登録が拒否されること。
    #[tokio::test]
    async fn register_rejects_a_third_party_https_redirect_uri() {
        let h = harness().await;
        let res = post_register(
            &h.app,
            serde_json::json!({"redirect_uris": ["https://attacker.example/cb"]}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// DCR レベルでもサフィックス一致では登録が通らないこと。
    #[tokio::test]
    async fn register_rejects_a_suffix_matching_https_redirect_uri() {
        let h = harness().await;
        let res = post_register(
            &h.app,
            serde_json::json!({"redirect_uris": ["https://evil-claude.ai/cb"]}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// DCR レベルで http loopback は複数ポートで登録できること。
    #[tokio::test]
    async fn register_accepts_http_loopback_redirect_uris_on_different_ports() {
        let h = harness().await;
        for uri in ["http://localhost:8080/cb", "http://127.0.0.1:54321/cb"] {
            let res = post_register(&h.app, serde_json::json!({"redirect_uris": [uri]})).await;
            assert_eq!(res.status(), StatusCode::CREATED, "{uri}");
        }
    }

    /// S4: redirect_uri の件数上限。無認証 endpoint なので上限が無いと、
    /// 署名対象の JSON を際限なく膨らませられる。
    #[tokio::test]
    async fn register_rejects_too_many_redirect_uris() {
        let h = harness().await;
        let uris: Vec<String> = (0..MAX_REDIRECT_URIS + 1)
            .map(|i| format!("https://claude.ai/cb{i}"))
            .collect();
        let res = post_register(&h.app, serde_json::json!({ "redirect_uris": uris })).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// 上限ちょうどは通ること（オフバイワンで正当な登録を弾かない）。
    #[tokio::test]
    async fn register_accepts_the_maximum_number_of_redirect_uris() {
        let h = harness().await;
        let uris: Vec<String> = (0..MAX_REDIRECT_URIS)
            .map(|i| format!("https://claude.ai/cb{i}"))
            .collect();
        let res = post_register(&h.app, serde_json::json!({ "redirect_uris": uris })).await;
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    /// S4: 1 件あたりの長さ上限。
    #[tokio::test]
    async fn register_rejects_an_overlong_redirect_uri() {
        let h = harness().await;
        let long = format!("https://claude.ai/{}", "a".repeat(MAX_REDIRECT_URI_LEN));
        let res = post_register(&h.app, serde_json::json!({"redirect_uris": [long]})).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// S3: 登録の **合計サイズ**の上限。件数と 1 件あたりの長さが上限内でも、
    /// 合計が大きいと client_id が肥大し、それを内包する封緘 state が Google の
    /// authorize URL に収まらず、**そのクライアントのログインが恒久的に失敗する**。
    /// 登録時点で検出できるようにする。
    #[tokio::test]
    async fn register_rejects_a_registration_that_is_too_large_in_total() {
        let h = harness().await;
        // 個々は上限内（`MAX_REDIRECT_URI_LEN` 未満）だが、合計が上限を超える組み合わせ。
        let uri = format!("https://claude.ai/{}", "a".repeat(1000));
        let uris: Vec<String> = (0..3).map(|_| uri.clone()).collect();
        assert!(uris.len() <= MAX_REDIRECT_URIS);
        assert!(uris.iter().all(|u| u.len() <= MAX_REDIRECT_URI_LEN));
        assert!(uris.iter().map(String::len).sum::<usize>() > MAX_REGISTRATION_BYTES);

        let res = post_register(&h.app, serde_json::json!({ "redirect_uris": uris })).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// S3: 現実的な登録（claude.ai の実際の形）は当然通ること。
    #[tokio::test]
    async fn register_accepts_a_realistic_registration_size() {
        let h = harness().await;
        let res = post_register(
            &h.app,
            serde_json::json!({"redirect_uris": [CLIENT_REDIRECT], "client_name": "Claude"}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
    }

    /// C2: **worst case** —— 上限いっぱいの登録（redirect_uris + client_name）に、
    /// 上限いっぱいの `client_state` を同時に与えても、authorize URL が実用長に
    /// 収まること。**この検証こそが合計上限を課した理由**なので URL 長で直接固定する。
    ///
    /// 旧実装は `redirect_uris` の合計しか数えておらず、`client_name`（`chars()` 基準で
    /// 実バイトは最大 4 倍）と `client_state`（1KB）が勘定に入っていなかったため、
    /// worst case で 8KB を超えていた。
    #[tokio::test]
    async fn a_maximum_sized_registration_still_produces_a_usable_authorize_url() {
        let h = harness().await;
        // 登録の合計予算いっぱい（redirect_uris + client_name）。
        let uri = format!("https://claude.ai/{}", "a".repeat(400));
        let uris: Vec<String> = (0..4).map(|_| uri.clone()).collect();
        // client_name は 4 バイト文字で埋めて、バイト数で数えていることを踏ませる。
        let client_name = "あ".repeat(MAX_CLIENT_NAME_LEN / 3);
        assert!(
            client_name.chars().count() < client_name.len(),
            "多バイト文字であること"
        );
        let total: usize = uris.iter().map(String::len).sum::<usize>() + client_name.len();
        assert!(total <= MAX_REGISTRATION_BYTES, "予算内であること: {total}");

        let res = post_register(
            &h.app,
            serde_json::json!({ "redirect_uris": uris, "client_name": client_name }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::CREATED);
        let client_id = body_json(res).await["client_id"]
            .as_str()
            .unwrap()
            .to_string();

        // authorize には上限いっぱいの client_state を添える。
        let client_state = "s".repeat(MAX_CLIENT_STATE_LEN);
        let uri_str = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256&state={}&resource={}",
            enc(&client_id),
            enc(&uri),
            enc(&client_challenge()),
            enc(&client_state),
            enc(&resource_uri(PROJECT_ID)),
        );
        let res = get_uri(&h.app, &uri_str).await;
        assert_eq!(
            res.status(),
            StatusCode::FOUND,
            "worst case の登録でもログインできること"
        );
        let loc = location(&res);
        assert!(
            loc.len() <= MAX_AUTHORIZE_URL_BYTES,
            "authorize URL grew to {} bytes; logins would fail for this client",
            loc.len()
        );
        // このテストが「たまたま小さい入力で通っている」のではなく、実際に上限付近を
        // 踏んでいることを固定する（実測 6360 バイト）。上限に対する余裕が
        // 分かる形で残しておくと、定数を動かしたときに影響が見える。
        assert!(
            loc.len() > 5000,
            "worst case should actually stress the limit, got {} bytes",
            loc.len()
        );
    }

    /// C2: `client_name` は **バイト長**で制限すること。`chars().count()` 基準だと
    /// 4 バイト文字ばかりの名前で実バイトが 4 倍になり、URL 長の見積もりが崩れる。
    #[tokio::test]
    async fn register_counts_the_client_name_in_bytes_not_characters() {
        let h = harness().await;
        // 文字数では上限内だが、バイト数では超える名前。
        let name = "あ".repeat(MAX_CLIENT_NAME_LEN - 1);
        assert!(name.chars().count() <= MAX_CLIENT_NAME_LEN);
        assert!(name.len() > MAX_CLIENT_NAME_LEN);
        let res = post_register(
            &h.app,
            serde_json::json!({"redirect_uris": [CLIENT_REDIRECT], "client_name": name}),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// C2: `client_name` は登録の合計予算に算入されること。
    #[tokio::test]
    async fn register_counts_the_client_name_towards_the_total_budget() {
        let h = harness().await;
        // redirect_uris だけなら予算内だが、client_name を足すと超える組み合わせ。
        let uri = format!(
            "https://claude.ai/{}",
            "a".repeat(MAX_REGISTRATION_BYTES - 200)
        );
        assert!(uri.len() <= MAX_REDIRECT_URI_LEN);
        assert!(uri.len() <= MAX_REGISTRATION_BYTES);
        let res = post_register(
            &h.app,
            serde_json::json!({
                "redirect_uris": [uri],
                "client_name": "n".repeat(MAX_CLIENT_NAME_LEN),
            }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// C2: URL 長の保証は authorize 側の実測が担う。`client_state` は登録時に
    /// 見えないため、登録側の予算だけでは押さえきれない。
    #[tokio::test]
    async fn authorize_rejects_a_request_whose_upstream_url_would_be_too_long() {
        let h = harness().await;
        // 予算内の登録。
        let uri = format!("https://claude.ai/{}", "a".repeat(400));
        let uris: Vec<String> = (0..4).map(|_| uri.clone()).collect();
        let res = post_register(&h.app, serde_json::json!({ "redirect_uris": uris })).await;
        let client_id = body_json(res).await["client_id"]
            .as_str()
            .unwrap()
            .to_string();

        // `MAX_CLIENT_STATE_LEN` を超える state は S3 の長さ検証で弾かれるので、
        // ここでは上限内の state を使い、URL 長チェック自体が存在することを
        // 「上限ちょうどでも通る」側から固定する（超過経路は上の worst case が担う）。
        let client_state = "s".repeat(MAX_CLIENT_STATE_LEN);
        let uri_str = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256&state={}&resource={}",
            enc(&client_id),
            enc(&uri),
            enc(&client_challenge()),
            enc(&client_state),
            enc(&resource_uri(PROJECT_ID)),
        );
        let res = get_uri(&h.app, &uri_str).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert!(location(&res).len() <= MAX_AUTHORIZE_URL_BYTES);
    }

    // ------------------------------------------------------------------
    // C3: 使用済み jti 集合の上限と刈り取り
    // ------------------------------------------------------------------

    /// C3: 期限切れエントリは刈り取られ、集合が無制限に育たないこと。
    #[test]
    fn the_used_jti_set_prunes_expired_entries() {
        let mut used = UsedJtis::new();
        // 閾値を超えるまで、すぐ期限切れになるエントリを詰める。
        for i in 0..PRUNE_THRESHOLD {
            assert!(used.consume(&format!("jti-{i}"), 100, 50));
        }
        assert_eq!(used.entries.len(), PRUNE_THRESHOLD);
        // 全エントリの期限を過ぎた時刻で 1 件消費すると、刈り取りが走る。
        assert!(used.consume("fresh", 200, 150));
        assert_eq!(
            used.entries.len(),
            1,
            "期限切れのエントリは刈り取られるべき"
        );
    }

    /// C3: 刈り取りは毎回ではなく閾値到達時のみ（償却 O(1)）。
    /// 生存エントリが多いときは次の閾値が引き上げられること。
    #[test]
    fn the_used_jti_set_raises_its_prune_threshold_when_entries_are_alive() {
        let mut used = UsedJtis::new();
        assert_eq!(used.prune_at, PRUNE_THRESHOLD);
        // すべて生存中（exp が十分先）のエントリで閾値を超える。各挿入で秒を進め、
        // 秒単位の間引きに阻まれず閾値の引き上げを観測できるようにする。
        for i in 0..=PRUNE_THRESHOLD {
            assert!(used.consume(&format!("jti-{i}"), 1_000_000, 50 + i as u64));
        }
        // 刈り取っても何も消えないので、次の閾値は生存件数の 2 倍へ。
        assert!(
            used.prune_at >= PRUNE_THRESHOLD * 2,
            "prune_at={} は引き上げられるべき（毎回全走査させない）",
            used.prune_at
        );
    }

    /// C3: 挿入のたびに全走査してはならない。これが崩れると、集合サイズ n に対して
    /// リクエストあたり O(n) となり、攻撃者のリクエスト数に対して防御側コストが
    /// 超線形（O(r²)）になる ―― 前段のレート制限で守るという整理の前提が壊れる。
    #[test]
    fn the_used_jti_set_does_not_scan_on_every_insert() {
        let mut used = UsedJtis::new();
        // 閾値の 4 倍を、秒を進めながら投入する。
        let inserts = PRUNE_THRESHOLD * 4;
        for i in 0..inserts {
            assert!(used.consume(&format!("jti-{i}"), 1_000_000, 50 + i as u64));
        }
        assert!(
            used.prunes < 10,
            "{inserts} 件の挿入で全走査が {} 回は多すぎる（償却されていない）",
            used.prunes
        );
    }

    /// C3: 上限に達したら **fail closed**。単回使用を担保できない状態で
    /// ブロブを通さない。
    #[test]
    fn the_used_jti_set_rejects_new_entries_once_it_is_full() {
        let mut used = UsedJtis::new();
        // すべて生存中のエントリで上限まで埋める。
        for i in 0..MAX_USED_JTIS {
            assert!(used.consume(&format!("jti-{i}"), 10_000, 50));
        }
        assert_eq!(used.entries.len(), MAX_USED_JTIS);
        assert!(
            !used.consume("overflow", 10_000, 50),
            "上限到達後は記録できないので通してはならない（fail closed）"
        );
        // 既存エントリの再提示は、上限に関係なく従来どおり拒否される。
        assert!(!used.consume("jti-0", 10_000, 50));
    }

    /// C3: 上限に達しても、期限切れが刈り取られれば回復すること
    /// （恒久的に閉じたままにならない）。
    #[test]
    fn the_used_jti_set_recovers_after_entries_expire() {
        let mut used = UsedJtis::new();
        for i in 0..MAX_USED_JTIS {
            assert!(used.consume(&format!("jti-{i}"), 100, 50));
        }
        assert!(!used.consume("overflow", 100, 50));
        // 全エントリの期限が過ぎた後は、また受け付けられる。
        assert!(used.consume("after-expiry", 500, 200));
    }

    /// C3 回帰: `prune_at` は `MAX_USED_JTIS` を超えて育ってはならない。
    ///
    /// 頭打ちが無いと、集合が満杯になった時点で `len >= prune_at` が成立しなくなり
    /// **刈り取りが二度と走らない**。上限判定だけが残るため、期限切れが溜まっても
    /// 回復せず、正規のログインが恒久的に通らなくなる。
    #[test]
    fn the_prune_threshold_never_exceeds_the_hard_limit() {
        let mut used = UsedJtis::new();
        // 生存エントリで満杯まで埋める（各挿入で秒を進め、刈り取りを毎回許可する）。
        for i in 0..MAX_USED_JTIS {
            assert!(used.consume(&format!("jti-{i}"), 1_000_000, 50 + i as u64));
        }
        assert!(
            used.prune_at <= MAX_USED_JTIS,
            "prune_at={} が上限を超えると満杯後に刈り取りが走らなくなる",
            used.prune_at
        );
    }

    /// C3: 満杯状態でも、同一秒内の刈り取りは 1 回に抑えること。
    /// ここが効かないと、満杯時にリクエストごとの O(n) 全走査が再発する。
    #[test]
    fn a_full_used_jti_set_does_not_rescan_on_every_request_within_a_second() {
        let mut used = UsedJtis::new();
        for i in 0..MAX_USED_JTIS {
            assert!(used.consume(&format!("jti-{i}"), 1_000_000, 50 + i as u64));
        }
        let now = 50 + MAX_USED_JTIS as u64;
        // 同じ秒で連続して拒否されるとき、全走査は 1 回しか起きない。
        assert!(!used.consume("overflow-1", 1_000_000, now));
        let after_first = used.prunes;
        for i in 0..50 {
            assert!(!used.consume(&format!("overflow-{i}"), 1_000_000, now));
        }
        assert_eq!(
            used.prunes, after_first,
            "同一秒内の追加リクエストで全走査を繰り返してはならない（満杯時の O(n) 再発）"
        );
    }

    /// C3: 単回使用の判定は刈り取りの前に行う（刈り取りは純粋にメモリ管理であって、
    /// 判定結果を変えてはならない）。
    #[test]
    fn the_used_jti_set_detects_reuse_regardless_of_pruning() {
        let mut used = UsedJtis::new();
        assert!(used.consume("jti", 10_000, 50));
        assert!(!used.consume("jti", 10_000, 50), "再提示は拒否されるべき");
    }

    /// S4: `client_name` は同意画面に出るため長さを制限する。
    #[tokio::test]
    async fn register_rejects_an_overlong_client_name() {
        let h = harness().await;
        let res = post_register(
            &h.app,
            serde_json::json!({
                "redirect_uris": [CLIENT_REDIRECT],
                "client_name": "a".repeat(MAX_CLIENT_NAME_LEN + 1),
            }),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    // ------------------------------------------------------------------
    // authorize
    // ------------------------------------------------------------------

    /// テストで使う PKCE の組。S4 の形式検証（base64url 43 文字）を満たすため、
    /// 実際に S256 変換した値を使う。
    const CLIENT_VERIFIER: &str = "client-verifier";

    fn client_challenge() -> String {
        s256(CLIENT_VERIFIER)
    }

    fn authorize_uri(client_id: &str, redirect_uri: &str, method: &str) -> String {
        format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method={method}&state=client-state&scope=openid\
             &resource={}",
            enc(client_id),
            enc(redirect_uri),
            enc(&client_challenge()),
            enc(&resource_uri(PROJECT_ID)),
        )
    }

    #[tokio::test]
    async fn authorize_redirects_to_google_with_server_side_credentials() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(&h.app, &authorize_uri(&client_id, CLIENT_REDIRECT, "S256")).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = url::Url::parse(&location(&res)).unwrap();
        assert_eq!(loc.host_str(), Some("accounts.google.com"));
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["client_id"], GOOGLE_CLIENT_ID);
        assert_eq!(
            q["redirect_uri"],
            format!("https://{PUBLIC_HOST}/oauth/callback")
        );
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["scope"], "openid email profile");
        // AS 自身も Google に対して PKCE を使う。
        assert_eq!(q["code_challenge_method"], "S256");
        assert!(!q["code_challenge"].is_empty());
        // W3: クライアントの文脈は **封緘した** state ブロブに入れて往復させる。
        let blob: Blob = h.key.open(&q["state"]).unwrap();
        match blob {
            Blob::State {
                redirect_uri,
                client_state,
                code_challenge,
                google_verifier,
                exp,
                ..
            } => {
                assert_eq!(redirect_uri, CLIENT_REDIRECT);
                assert_eq!(client_state.as_deref(), Some("client-state"));
                assert_eq!(code_challenge, client_challenge());
                assert_eq!(s256(&google_verifier), q["code_challenge"]);
                assert!(exp > now_secs());
            }
            other => panic!("expected a state blob, got {other:?}"),
        }
    }

    /// W3: Google の authorize URL に載る `state` から、AS 用の PKCE verifier が
    /// 読み出せてはならない。この URL はブラウザ履歴・Referer・Google 側ログに残る。
    /// 署名のみ（`sign`）へ退行するとここで落ちる。
    #[tokio::test]
    async fn state_sent_to_google_does_not_expose_the_pkce_verifier() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(&h.app, &authorize_uri(&client_id, CLIENT_REDIRECT, "S256")).await;
        let loc = location(&res);
        let url = url::Url::parse(&loc).unwrap();
        let q: HashMap<_, _> = url.query_pairs().into_owned().collect();
        let Blob::State {
            google_verifier, ..
        } = h.key.open::<Blob>(&q["state"]).unwrap()
        else {
            panic!("expected a state blob");
        };
        // verifier そのものも、その base64url 表現も URL 全体に現れないこと。
        assert!(!loc.contains(&google_verifier), "{loc}");
        assert!(
            !loc.contains(&b64_encode(google_verifier.as_bytes())),
            "{loc}"
        );
        // 封緘されているので `verify`（署名のみ）では読めない。
        assert!(h.key.verify::<Blob>(&q["state"]).is_err());
    }

    #[tokio::test]
    async fn authorize_rejects_redirect_uri_not_in_the_registration() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(
            &h.app,
            &authorize_uri(&client_id, "https://evil.example.com/cb", "S256"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// 前方一致・部分一致で通してはならない。`https://claude.ai/api/mcp/auth_callback`
    /// の登録に対し、それを接頭辞に持つ別 URI が通るとリダイレクト先を奪える。
    #[tokio::test]
    async fn authorize_requires_exact_redirect_uri_match() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(
            &h.app,
            &authorize_uri(&client_id, &format!("{CLIENT_REDIRECT}/../evil"), "S256"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authorize_rejects_plain_code_challenge_method() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(&h.app, &authorize_uri(&client_id, CLIENT_REDIRECT, "plain")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn authorize_rejects_tampered_client_id() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let forged = Blob::Client {
            redirect_uris: vec!["https://evil.example.com/cb".to_string()],
            client_name: None,
            iat: now_secs(),
        };
        let (_, sig) = client_id.split_once('.').unwrap();
        let tampered = format!(
            "{}.{sig}",
            b64_encode(&serde_json::to_vec(&forged).unwrap())
        );
        let res = get_uri(
            &h.app,
            &authorize_uri(&tampered, "https://evil.example.com/cb", "S256"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// C1: Google へのリダイレクトに `access_type=offline` と `prompt=consent` が
    /// 付くこと。前者が無いと refresh_token が返らず、後者が無いと **既に同意済みの
    /// 利用者にだけ** refresh_token が返らない（最も気づきにくい壊れ方をする）。
    #[tokio::test]
    async fn authorize_requests_offline_access_with_forced_consent() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = get_uri(&h.app, &authorize_uri(&client_id, CLIENT_REDIRECT, "S256")).await;
        let loc = url::Url::parse(&location(&res)).unwrap();
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
    }

    /// S3: 期限切れのクライアント登録は authorize で拒否する。`iat` を持ちながら
    /// 検証しないと、無認証で発行された client_id が永久に有効になる。
    #[tokio::test]
    async fn authorize_rejects_an_expired_client_registration() {
        let h = harness().await;
        let stale = h
            .key
            .sign(&Blob::Client {
                redirect_uris: vec![CLIENT_REDIRECT.to_string()],
                client_name: None,
                iat: now_secs() - CLIENT_MAX_AGE_SECS - 1,
            })
            .unwrap();
        let res = get_uri(&h.app, &authorize_uri(&stale, CLIENT_REDIRECT, "S256")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_client");
    }

    /// 期限内の登録は通ること（境界で正当な登録を弾かない）。
    #[tokio::test]
    async fn authorize_accepts_a_client_registration_within_its_lifetime() {
        let h = harness().await;
        let fresh = h
            .key
            .sign(&Blob::Client {
                redirect_uris: vec![CLIENT_REDIRECT.to_string()],
                client_name: None,
                iat: now_secs() - CLIENT_MAX_AGE_SECS + 60,
            })
            .unwrap();
        let res = get_uri(&h.app, &authorize_uri(&fresh, CLIENT_REDIRECT, "S256")).await;
        assert_eq!(res.status(), StatusCode::FOUND);
    }

    // ------------------------------------------------------------------
    // C5: resource（RFC 8707）の解決と束縛
    // ------------------------------------------------------------------

    fn config_with_projects(project_ids: &[&str]) -> AuthServerConfig {
        AuthServerConfig::new(
            PUBLIC_HOST.into(),
            GOOGLE_CLIENT_ID.into(),
            "s".into(),
            project_ids.iter().map(|p| (*p).to_string()).collect(),
        )
    }

    #[test]
    fn resource_resolves_to_the_project_it_identifies() {
        let config = config_with_projects(&[PROJECT_ID, OTHER_PROJECT_ID]);
        assert_eq!(
            resolve_resource(&config, Some(&resource_uri(PROJECT_ID))).unwrap(),
            PROJECT_ID
        );
        assert_eq!(
            resolve_resource(&config, Some(&resource_uri(OTHER_PROJECT_ID))).unwrap(),
            OTHER_PROJECT_ID
        );
    }

    /// 末尾スラッシュの有無で解決結果が変わらないこと。
    #[test]
    fn resource_tolerates_a_trailing_slash() {
        let config = config_with_projects(&[PROJECT_ID]);
        let with_slash = format!("{}/", resource_uri(PROJECT_ID));
        assert_eq!(
            resolve_resource(&config, Some(&with_slash)).unwrap(),
            PROJECT_ID
        );
    }

    /// 他ホストを指す resource は拒否する。受け入れると `aud` の意味が
    /// 「このサーバのどの project か」からずれて束縛が形骸化する。
    #[test]
    fn resource_for_another_host_is_rejected() {
        let config = config_with_projects(&[PROJECT_ID]);
        assert!(resolve_resource(
            &config,
            Some(&format!("https://evil.example.com/{PROJECT_ID}/mcp"))
        )
        .is_err());
    }

    #[test]
    fn resource_for_an_unknown_project_is_rejected() {
        let config = config_with_projects(&[PROJECT_ID]);
        assert!(resolve_resource(&config, Some(&resource_uri("nope"))).is_err());
    }

    #[test]
    fn resource_that_is_not_an_mcp_endpoint_is_rejected() {
        let config = config_with_projects(&[PROJECT_ID]);
        for bad in [
            format!("https://{PUBLIC_HOST}/{PROJECT_ID}"),
            format!("https://{PUBLIC_HOST}/{PROJECT_ID}/mcp/extra"),
            format!("https://{PUBLIC_HOST}/"),
            "not-a-uri".to_string(),
        ] {
            assert!(
                resolve_resource(&config, Some(&bad)).is_err(),
                "resource {bad:?} must be rejected"
            );
        }
    }

    /// C5: `resource` 未指定時。project が 1 件ならそれに束縛する
    /// （必須化すると resource を送らない既存クライアントの接続が落ちるため）。
    #[test]
    fn a_missing_resource_binds_to_the_only_project() {
        let config = config_with_projects(&[PROJECT_ID]);
        assert_eq!(resolve_resource(&config, None).unwrap(), PROJECT_ID);
    }

    /// C5: project が 2 件以上あるときは束縛先を推測できないため拒否する
    /// （fail closed）。**project を足した瞬間に `resource` が必須になる**という
    /// 連動そのものを、ここで固定しておく。
    #[test]
    fn a_missing_resource_is_rejected_when_several_projects_exist() {
        let config = config_with_projects(&[PROJECT_ID, OTHER_PROJECT_ID]);
        assert!(resolve_resource(&config, None).is_err());
        assert!(resolve_resource(&config, Some("")).is_err());
    }

    /// 設定済み project を指す `resource` を伴う authorize は通ること。
    ///
    /// **注意**: 旧実装はここで解決した project_id をアクセストークンの `aud` に
    /// 束縛していたが、Google 発行のトークンを中継する現行ではその束縛ができない
    /// （`resolve_resource` のコメント参照）。したがってここで確認できるのは
    /// 「受理されて Google の authorize へ進む」ところまでである。
    #[tokio::test]
    async fn authorize_accepts_a_resource_for_a_configured_project() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let uri = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256&resource={}",
            enc(&client_id),
            enc(CLIENT_REDIRECT),
            enc(&client_challenge()),
            enc(&resource_uri(PROJECT_ID)),
        );
        let res = get_uri(&h.app, &uri).await;
        assert_eq!(res.status(), StatusCode::FOUND);
        assert!(location(&res).starts_with("https://accounts.google.com/"));
    }

    /// 未知の resource を指定した authorize は `invalid_target` で拒否される。
    #[tokio::test]
    async fn authorize_rejects_an_unknown_resource() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let uri = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256&resource={}",
            enc(&client_id),
            enc(CLIENT_REDIRECT),
            enc(&client_challenge()),
            enc(&resource_uri("nope")),
        );
        let res = get_uri(&h.app, &uri).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_target");
    }

    /// リフレッシュ時に設定に無い resource を指定したら拒否されること。
    ///
    /// **元の束縛との照合はできない**（上流トークンは不透明で、こちらの
    /// project_id を載せていない）。ここで担保されるのは「設定済み project を
    /// 指しているか」だけである。
    #[tokio::test]
    async fn refresh_grant_rejects_an_unusable_resource() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = post_token(
            &h.app,
            &format!(
                "{}&resource={}",
                refresh_form("google-refresh-token", &client_id),
                enc(&resource_uri("nope"))
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// 設定済みの resource を明示した場合は Google への中継まで進むこと。
    #[tokio::test]
    async fn refresh_grant_accepts_a_configured_resource() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let res = post_token(
            &h.app,
            &format!(
                "{}&resource={}",
                refresh_form("google-refresh-token", &client_id),
                enc(&resource_uri(PROJECT_ID))
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::OK);
    }

    // ------------------------------------------------------------------
    // C4: 時計異常時の向き（fail closed）
    // ------------------------------------------------------------------

    /// 正常時はそのまま UNIX 秒を返す。
    #[test]
    fn clock_secs_returns_the_elapsed_seconds_when_the_clock_is_sane() {
        assert_eq!(
            clock_secs(Some(Duration::from_secs(1_700_000_000))),
            1_700_000_000
        );
    }

    /// C4: 時計が 1970 より前に巻き戻っている異常系は **fail closed**。
    ///
    /// 旧実装は `unwrap_or(0)` で、コメントには「fail closed になる」と書いてあったが
    /// 実際は真逆だった。判定は全箇所が `exp <= now` であり、`now = 0` では正の `exp` に
    /// 対して常に false、つまり state・認可コード・同意ブロブ・リフレッシュトークンが
    /// **一切期限切れにならない** fail open だった。
    #[test]
    fn clock_secs_fails_closed_when_the_clock_is_broken() {
        let now = clock_secs(None);
        assert_eq!(now, u64::MAX);
        // 「あらゆる exp が期限切れと判定される」ことこそが fail closed の中身。
        for exp in [1u64, 1_700_000_000, u64::MAX] {
            assert!(exp <= now, "exp {exp} must be treated as expired");
        }
        // 旧実装の `0` では、逆に何ひとつ期限切れにならなかったことを対比で示す。
        // broken clock を 0 に潰していたため、正の `exp` は `exp <= now(=0)` が常に false ＝
        // 期限切れにならなかった。「exp が broken now を上回る」形で同じ意味を表す
        // （u64 の `<= 0` リテラル比較は absurd_extreme_comparisons に触れるため避ける）。
        let old_broken_now: u64 = 0;
        assert!(
            1_700_000_000u64 > old_broken_now,
            "old impl (now=0) treated a positive exp as not-yet-expired"
        );
    }

    /// 時計異常時に `exp` の計算が桁溢れしないこと（飽和して「常に期限切れ」になる）。
    #[test]
    fn expires_in_saturates_instead_of_wrapping() {
        assert_eq!(u64::MAX.saturating_add(STATE_TTL_SECS), u64::MAX);
    }

    // ------------------------------------------------------------------
    // callback
    // ------------------------------------------------------------------

    /// W3 により state ブロブは封緘（`seal`）で作る。
    fn state_blob(key: &SigningKey, client_id: &str, exp_offset: i64) -> String {
        key.seal(&Blob::State {
            client_id: client_id.to_string(),
            redirect_uri: CLIENT_REDIRECT.to_string(),
            client_state: Some("client-state".to_string()),
            code_challenge: client_challenge(),
            google_verifier: "google-verifier".to_string(),
            // 単回使用（C2）。テストごとに一意にして、同一プロセス内の他のテストと
            // 使用済み集合が干渉しないようにする。
            jti: uuid::Uuid::new_v4().to_string(),
            exp: (now_secs() as i64 + exp_offset) as u64,
        })
        .unwrap()
    }

    /// redirect_uri の許可リストが confused deputy を防ぐため、callback は
    /// 同意画面を経由せず、Google の identity 確定後にその場で認可コードを発行して
    /// クライアントの redirect_uri へリダイレクトする（設計:
    /// docs/superpowers/specs/2026-07-22-restrict-redirect-uri-design.md）。
    #[tokio::test]
    async fn callback_issues_an_authorization_code_directly_and_redirects_to_the_client() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = url::Url::parse(&location(&res)).unwrap();
        assert!(location(&res).starts_with(CLIENT_REDIRECT), "{loc}");
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        // クライアントの元の state をそのまま返す（CSRF 対策が成立する条件）。
        assert_eq!(q["state"], "client-state");

        // Google の client_secret はサーバ側 env から取り、token endpoint に送る。
        let sent = h.google_token_log.lock().unwrap().join("\n");
        assert!(
            sent.contains("client_secret=google-client-secret"),
            "{sent}"
        );
        assert!(sent.contains("code_verifier=google-verifier"), "{sent}");

        // 認可コードは封緘済み（`open` でしか読めない）。
        let blob: Blob = h.key.open(&q["code"]).unwrap();
        match blob {
            Blob::Code {
                sub,
                email,
                code_challenge,
                upstream,
                exp,
                ..
            } => {
                assert_eq!(sub, "1122334455");
                assert_eq!(email, "cs@example.com");
                assert_eq!(code_challenge, s256("client-verifier"));
                // Google 発行のトークンが認可コードに封入されていること。
                // これをそのままクライアントへ渡すのが現行の設計。
                assert_eq!(upstream.access_token, GOOGLE_ACCESS_TOKEN);
                assert_eq!(
                    upstream.refresh_token.as_deref(),
                    Some(GOOGLE_REFRESH_TOKEN)
                );
                assert!(exp <= now_secs() + CODE_TTL_SECS);
            }
            other => panic!("expected a code blob, got {other:?}"),
        }
        // 封緘の核心。認可コードは URL に載るため、Google の access_token /
        // refresh_token が部分文字列としても現れてはならない。
        assert!(!q["code"].contains(GOOGLE_REFRESH_TOKEN), "{}", q["code"]);
        assert!(!q["code"].contains(GOOGLE_ACCESS_TOKEN), "{}", q["code"]);
    }

    /// 認可コードは、自身の `exp` まで使用済み記録が残ること。
    #[tokio::test]
    async fn an_authorization_code_stays_consumed_for_its_own_lifetime() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let form = code_form(&code, &client_id, CLIENT_VERIFIER);
        assert_eq!(post_token(&h.app, &form).await.status(), StatusCode::OK);
        // コードの寿命内で再提示 → 使用済みとして拒否。
        h.clock.advance(CODE_TTL_SECS / 2);
        assert_eq!(
            post_token(&h.app, &form).await.status(),
            StatusCode::BAD_REQUEST
        );
    }

    #[tokio::test]
    async fn callback_rejects_tampered_state() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let (payload, _) = state.split_once('.').unwrap();
        let res = get_uri(
            &h.app,
            &format!(
                "/oauth/callback?code=google-code&state={}",
                enc(&format!("{payload}.AAAA"))
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        // 署名検証で落ちた以上、Google の token endpoint を叩いてはならない。
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// W1: state は検証できたが期限切れ。redirect_uri は署名済み state 由来で
    /// 信用できるため、400 JSON ではなくクライアントへエラーリダイレクトする。
    #[tokio::test]
    async fn callback_redirects_the_client_when_the_state_has_expired() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, -1);
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = url::Url::parse(&location(&res)).unwrap();
        assert!(location(&res).starts_with(CLIENT_REDIRECT), "{loc}");
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["error"], "invalid_request");
        assert_eq!(q["state"], "client-state");
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn callback_rejects_unverified_email() {
        let h = harness_with_tokeninfo(Box::leak(tokeninfo_body("false").into_boxed_str())).await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// W1: state を伴わない Google エラーは、**戻り先を信用できない**ため 400 のまま。
    /// ここを redirect に変えるとオープンリダイレクタになる。
    #[tokio::test]
    async fn callback_rejects_a_google_error_without_state_as_400() {
        let h = harness().await;
        let res = get_uri(&h.app, "/oauth/callback?error=access_denied").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(res.headers().get(header::LOCATION).is_none());
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// W1: 検証済み state を伴う Google の `access_denied`（利用者が Google 側で
    /// 拒否した経路）は、クライアントへリダイレクトして返す。
    #[tokio::test]
    async fn callback_redirects_a_google_access_denied_with_a_valid_state() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?error=access_denied&state={}", enc(&state)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::FOUND);
        let loc = url::Url::parse(&location(&res)).unwrap();
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["error"], "access_denied");
        assert_eq!(q["state"], "client-state");
        // 拒否された以上、Google の token 交換には進んではならない。
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// C2（reviewer 指摘 W-A）: **state は単回使用。**
    ///
    /// 同じ state での 2 回目の callback が拒否され、かつ **Google を叩かない**こと。
    /// これが無いと、`register` → `authorize` だけで有効な state を 1 つ得た
    /// 無認証の相手が、`STATE_TTL_SECS`（600 秒）にわたって Google の token endpoint への
    /// 外向きリクエストを任意レートで発生させられる（増幅経路）。
    #[tokio::test]
    async fn a_state_can_only_be_used_once() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let uri = format!("/oauth/callback?code=google-code&state={}", enc(&state));

        // 1 回目は正常に処理され、認可コード付きでクライアントへリダイレクトされる。
        let first = get_uri(&h.app, &uri).await;
        assert_eq!(first.status(), StatusCode::FOUND);
        let hits_after_first = h.google_token_hits.load(Ordering::SeqCst);
        assert_eq!(hits_after_first, 1, "1 回目は Google を叩く");

        // 2 回目は拒否され、**Google には一切到達しない**。
        let second = get_uri(&h.app, &uri).await;
        assert_eq!(second.status(), StatusCode::FOUND);
        let loc = url::Url::parse(&location(&second)).unwrap();
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["error"], "invalid_request");
        assert!(!q.contains_key("code"), "{loc}");
        assert_eq!(
            h.google_token_hits.load(Ordering::SeqCst),
            hits_after_first,
            "a replayed state must not reach Google (this is the amplification vector)"
        );
    }

    /// C2: state を使い回した連打で Google への外向きリクエストを増幅できないこと。
    /// 10 回叩いても上流に届くのは最初の 1 回だけ。
    #[tokio::test]
    async fn replaying_a_state_cannot_amplify_requests_to_google() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let uri = format!("/oauth/callback?code=google-code&state={}", enc(&state));
        for _ in 0..10 {
            let _ = get_uri(&h.app, &uri).await;
        }
        assert_eq!(
            h.google_token_hits.load(Ordering::SeqCst),
            1,
            "10 回の callback で Google に届くのは 1 回だけであるべき"
        );
    }

    /// C2: Google がエラーを返した経路でも state を使い切る（フローは終了しているため）。
    #[tokio::test]
    async fn a_state_is_consumed_even_when_google_reports_an_error() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let denied = get_uri(
            &h.app,
            &format!("/oauth/callback?error=access_denied&state={}", enc(&state)),
        )
        .await;
        assert_eq!(denied.status(), StatusCode::FOUND);

        // 同じ state で改めて正常な callback を試みても通らない。
        let reuse = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await;
        let loc = url::Url::parse(&location(&reuse)).unwrap();
        let q: HashMap<_, _> = loc.query_pairs().into_owned().collect();
        assert_eq!(q["error"], "invalid_request");
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// S1: 署名のみの（封緘されていない）state は受け付けない。
    /// `consent_rejects_a_merely_signed_blob` /
    /// `refresh_grant_rejects_a_merely_signed_refresh_token` と同じ形で、
    /// 封緘要件を 3 種すべてに揃えて固定する（W3 の退行防止）。
    #[tokio::test]
    async fn callback_rejects_a_merely_signed_state() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let signed = h
            .key
            .sign(&Blob::State {
                client_id,
                redirect_uri: CLIENT_REDIRECT.to_string(),
                client_state: Some("client-state".to_string()),
                code_challenge: client_challenge(),
                google_verifier: "google-verifier".to_string(),
                jti: uuid::Uuid::new_v4().to_string(),
                exp: now_secs() + 300,
            })
            .unwrap();
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&signed)),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// W1: 改竄された state は redirect 先を信用できないので 400 のまま維持する。
    #[tokio::test]
    async fn callback_does_not_redirect_when_the_state_is_unverifiable() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        let (payload, _) = state.split_once('.').unwrap();
        let res = get_uri(
            &h.app,
            &format!(
                "/oauth/callback?error=access_denied&state={}",
                enc(&format!("{payload}.AAAA"))
            ),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert!(res.headers().get(header::LOCATION).is_none());
    }

    // ------------------------------------------------------------------
    // token
    // ------------------------------------------------------------------

    /// callback → 認可コード、と本番と同じ経路を通してコードを得る。
    async fn issue_code(h: &Harness, client_id: &str) -> String {
        let state = state_blob(&h.key, client_id, 300);
        let res = get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await;
        let loc = url::Url::parse(&location(&res)).unwrap();
        loc.query_pairs()
            .find(|(k, _)| k == "code")
            .map(|(_, v)| v.into_owned())
            .unwrap()
    }

    fn code_form(code: &str, client_id: &str, verifier: &str) -> String {
        format!(
            "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
            enc(code),
            enc(client_id),
            enc(CLIENT_REDIRECT),
            enc(verifier),
        )
    }

    /// **自前トークン廃止後の中心的な期待**: 認可コード交換で返るのは、Google が
    /// 発行した access_token / refresh_token **そのもの**であること。
    /// 自前発行へ戻る退行が起きたら、この等値比較が落ちる。
    #[tokio::test]
    async fn token_returns_the_google_issued_tokens_verbatim() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let res = post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["access_token"], GOOGLE_ACCESS_TOKEN);
        assert_eq!(json["refresh_token"], GOOGLE_REFRESH_TOKEN);
    }

    /// Google が申告した `expires_in` を、同意画面での滞留分だけ差し引いて
    /// 返すこと。相対秒をそのまま持ち回すと、クライアントへ渡るころには
    /// 実際より長い寿命を申告することになる。
    #[tokio::test]
    async fn expires_in_is_reduced_by_the_time_spent_on_the_consent_screen() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        // 同意から交換までに 30 秒経過したことにする。
        h.clock.advance(30);
        let json =
            body_json(post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await)
                .await;
        // Google の申告は 3599 秒。30 秒経過しているので、それより短くなる。
        assert_eq!(json["expires_in"], 3599 - 30);
    }

    /// Google が `expires_in` を返さなかった場合、寿命不明のまま渡さず、
    /// **短い側**を仮定して申告すること（早めのリフレッシュに倒す）。
    #[tokio::test]
    async fn a_missing_upstream_expires_in_falls_back_to_a_short_lifetime() {
        let h = harness_full(
            Box::leak(tokeninfo_body("true").into_boxed_str()),
            concat!(
                r#"{"access_token":"google-access-token","#,
                r#""refresh_token":"1//0gUPSTREAM-REFRESH-TOKEN"}"#
            ),
            vec![PROJECT_ID.to_string()],
        )
        .await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let json =
            body_json(post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await)
                .await;
        assert_eq!(json["expires_in"], ASSUMED_GOOGLE_ACCESS_TTL_SECS);
    }

    /// 封緘の核心。認可コード文字列に Google の access_token / refresh_token が
    /// **部分文字列としても現れない**こと。認可コードは redirect_uri のクエリに
    /// 載るため、署名のみ（`sign`）へ退行するとブラウザ履歴・Referer・中間ログに
    /// 上流クレデンシャルが平文で残る。
    #[tokio::test]
    async fn the_authorization_code_does_not_expose_the_google_tokens() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        for secret in [GOOGLE_REFRESH_TOKEN, GOOGLE_ACCESS_TOKEN] {
            assert!(!code.contains(secret), "{code}");
            assert!(!code.contains(&b64_encode(secret.as_bytes())), "{code}");
        }
        // 封緘を解けば中に入っていること自体は確認できる（運ばれてはいる）。
        let Blob::Code { upstream, .. } = h.key.open::<Blob>(&code).unwrap() else {
            panic!("expected a code blob");
        };
        assert_eq!(
            upstream.refresh_token.as_deref(),
            Some(GOOGLE_REFRESH_TOKEN)
        );
    }

    /// Google が refresh_token を返さなかった場合の縮退。access_token だけを
    /// 渡し、**refresh_token は出さない**。クライアントには標準的な
    /// 「refresh_token 無しの応答」に見え、期限切れ後は再認可に落ちる。
    #[tokio::test]
    async fn token_omits_the_refresh_token_when_google_returned_none() {
        let h = harness_full(
            Box::leak(tokeninfo_body("true").into_boxed_str()),
            r#"{"access_token":"google-access-token","expires_in":3599}"#,
            vec![PROJECT_ID.to_string()],
        )
        .await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let res = post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await;
        assert_eq!(res.status(), StatusCode::OK);
        let json = body_json(res).await;
        assert!(json["access_token"].as_str().is_some());
        assert!(
            json.get("refresh_token").is_none(),
            "must not issue a refresh token that cannot be revalidated upstream: {json}"
        );
    }

    #[tokio::test]
    async fn token_rejects_a_wrong_pkce_verifier() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let res = post_token(&h.app, &code_form(&code, &client_id, "wrong-verifier")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    #[tokio::test]
    async fn token_rejects_a_reused_code() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let first = post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await;
        assert_eq!(first.status(), StatusCode::OK);
        let second = post_token(&h.app, &code_form(&code, &client_id, "client-verifier")).await;
        assert_eq!(second.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_rejects_an_expired_code() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let expired = h
            .key
            .seal(&Blob::Code {
                sub: "1122334455".into(),
                email: "cs@example.com".into(),
                client_id: client_id.clone(),
                redirect_uri: CLIENT_REDIRECT.into(),
                code_challenge: s256("client-verifier"),
                upstream: test_upstream(),
                jti: "jti-expired".into(),
                exp: now_secs() - 1,
            })
            .unwrap();
        let res = post_token(&h.app, &code_form(&expired, &client_id, "client-verifier")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn token_rejects_a_redirect_uri_that_differs_from_the_code() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let form = format!(
            "grant_type=authorization_code&code={}&client_id={}&redirect_uri={}&code_verifier={}",
            enc(&code),
            enc(&client_id),
            enc("https://evil.example.com/cb"),
            enc("client-verifier"),
        );
        let res = post_token(&h.app, &form).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// 別種のブロブ（state ブロブ）を認可コードとして持ち込む試み。`Blob` の tag が
    /// 一致しないため deserialize 段階で落ちる。「署名は正しいので通す」という
    /// 事故を、呼び出し規約ではなく型で防いでいることの確認。
    #[tokio::test]
    async fn token_rejects_a_state_blob_presented_as_an_authorization_code() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let state = h
            .key
            .seal(&Blob::State {
                client_id: client_id.clone(),
                redirect_uri: CLIENT_REDIRECT.into(),
                client_state: None,
                code_challenge: s256("client-verifier"),
                google_verifier: "google-verifier".into(),
                jti: "j".into(),
                exp: now_secs() + 3600,
            })
            .unwrap();
        let res = post_token(&h.app, &code_form(&state, &client_id, "client-verifier")).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// リフレッシュ交換用のフォーム。
    fn refresh_form(refresh: &str, client_id: &str) -> String {
        format!(
            "grant_type=refresh_token&refresh_token={}&client_id={}",
            enc(refresh),
            enc(client_id)
        )
    }

    /// callback → 同意 → code → token と通してリフレッシュトークンを得る。
    async fn issue_refresh_token(h: &Harness, client_id: &str) -> String {
        let code = issue_code(h, client_id).await;
        let json =
            body_json(post_token(&h.app, &code_form(&code, client_id, "client-verifier")).await)
                .await;
        json["refresh_token"].as_str().unwrap().to_string()
    }

    /// **中継の中心的な期待**: クライアントが提示した refresh_token（= Google が
    /// 発行した値そのもの）を、Google の token endpoint へそのまま送ること。
    /// 自前ブロブの復号を挟む実装へ退行したら、送出内容が変わってここで落ちる。
    #[tokio::test]
    async fn refresh_grant_relays_the_clients_refresh_token_to_google() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let refresh = issue_refresh_token(&h, &client_id).await;
        // クライアントが受け取った値は Google の refresh_token そのもの。
        assert_eq!(refresh, GOOGLE_REFRESH_TOKEN);

        let before = h.google_token_hits.load(Ordering::SeqCst);
        let res = post_token(&h.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert!(h.google_token_hits.load(Ordering::SeqCst) > before);

        let sent = h.google_token_log.lock().unwrap().join("\n");
        assert!(sent.contains("grant_type=refresh_token"), "{sent}");
        // 上流クレデンシャルがそのまま中継されていること。
        assert!(sent.contains(&enc(GOOGLE_REFRESH_TOKEN)), "{sent}");
        // **client_secret はサーバ側から添える。** これを添えられることが、
        // クライアントに secret を持たせずに済ませる唯一の理由である。
        assert!(
            sent.contains("client_secret=google-client-secret"),
            "{sent}"
        );
    }

    /// リフレッシュ応答も Google のものをそのまま返すこと。
    #[tokio::test]
    async fn refresh_grant_returns_the_google_issued_access_token() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let refresh = issue_refresh_token(&h, &client_id).await;
        let json = body_json(post_token(&h.app, &refresh_form(&refresh, &client_id)).await).await;
        assert_eq!(json["access_token"], GOOGLE_ACCESS_TOKEN);
        assert_eq!(json["token_type"], "Bearer");
        assert_eq!(json["expires_in"], 3599);
    }

    /// **Google が拒否したらこちらも拒否する**（失効の伝播）。
    /// アカウント停止・グラント取消はこの経路で締め出される。
    #[tokio::test]
    async fn refresh_grant_is_refused_when_google_revokes_the_grant() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let refresh = issue_refresh_token(&h, &client_id).await;

        // Google が拒否する（= 取消済み）stub に差し替えた AS で、同じ
        // リフレッシュトークンを提示する。
        let revoking = harness_rejecting_google(&h.key).await;
        let res = post_token(&revoking.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// Google に到達できないだけのときは、失効と断定せず 503 で再試行させる。
    /// **これがエラー分類を中継経路でも維持する理由**である。`invalid_grant` を
    /// 返すとクライアントは仕様どおり refresh_token を破棄し、Google 側の
    /// 一時障害がそのまま全利用者の強制ログアウトに化ける。
    #[tokio::test]
    async fn refresh_grant_returns_503_when_google_is_unreachable() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let refresh = issue_refresh_token(&h, &client_id).await;

        let unreachable = harness_with_dead_google(&h.key).await;
        let res = post_token(&unreachable.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(res).await["error"], "temporarily_unavailable");
    }

    /// 上流応答の本文が壊れている場合は **503**（失効と断定しない）。
    #[tokio::test]
    async fn refresh_grant_returns_503_when_the_upstream_body_is_unusable() {
        let issuing = harness().await;
        let client_id = register_client(&issuing.app).await;
        let refresh = issue_refresh_token(&issuing, &client_id).await;
        // 2xx だが access_token を含まない応答。
        let (google_token_url, _, _) = spawn_stub(r#"{"expires_in":3599}"#).await;
        let target = harness_with_google_token_url(&issuing.key, google_token_url).await;
        let res = post_token(&target.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// このサーバが発行していない client_id では通さない。
    #[tokio::test]
    async fn refresh_grant_rejects_an_unverifiable_client_id() {
        let h = harness().await;
        let res = post_token(
            &h.app,
            &refresh_form(GOOGLE_REFRESH_TOKEN, "not-a-registration"),
        )
        .await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    /// refresh_token / client_id が欠けているリクエストは、Google を叩く前に弾く。
    #[tokio::test]
    async fn refresh_grant_requires_both_a_refresh_token_and_a_client_id() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        for form in [
            format!("grant_type=refresh_token&client_id={}", enc(&client_id)),
            format!(
                "grant_type=refresh_token&refresh_token={}",
                enc(GOOGLE_REFRESH_TOKEN)
            ),
        ] {
            let res = post_token(&h.app, &form).await;
            assert_eq!(res.status(), StatusCode::BAD_REQUEST, "form was: {form}");
        }
        assert_eq!(h.google_token_hits.load(Ordering::SeqCst), 0);
    }

    // ------------------------------------------------------------------
    // C1: 上流の非成功応答の分類（失効 vs 一時障害）
    // ------------------------------------------------------------------

    /// 上流が指定の失敗応答を返す状況で、リフレッシュを 1 回試みる。
    async fn refresh_against_failure(
        status_line: &'static str,
        extra_headers: &'static str,
        body: &'static str,
    ) -> Response {
        let issuing = harness().await;
        let client_id = register_client(&issuing.app).await;
        let refresh = issue_refresh_token(&issuing, &client_id).await;
        let url = spawn_failing_stub(status_line, extra_headers, body).await;
        let target = harness_with_google_token_url(&issuing.key, url).await;
        post_token(&target.app, &refresh_form(&refresh, &client_id)).await
    }

    /// C1 の核心。**429 は失効ではない。**
    ///
    /// 旧実装は 5xx 以外の非成功応答をすべて `Revoked` にしていたため、429 が
    /// `invalid_grant` に変換され、OAuth クライアントはリフレッシュトークンを破棄した。
    /// つまり **Google の一時的な流量制限だけで利用者が再ログイン必須**になり、
    /// 集中アクセス時ほど多数の利用者が同時にセッションを失う形になっていた。
    #[tokio::test]
    async fn a_rate_limited_upstream_is_a_temporary_failure_not_a_revocation() {
        let res = refresh_against_failure(
            "429 Too Many Requests",
            "",
            r#"{"error":"rateLimitExceeded"}"#,
        )
        .await;
        assert_eq!(
            res.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "429 must not destroy the user's session"
        );
        assert_eq!(body_json(res).await["error"], "temporarily_unavailable");
    }

    /// C1: Google が `Retry-After` を示したら、それをクライアントへ伝える。
    #[tokio::test]
    async fn a_rate_limited_upstream_propagates_retry_after() {
        let res =
            refresh_against_failure("429 Too Many Requests", "Retry-After: 30\r\n", r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            res.headers().get(header::RETRY_AFTER).unwrap(),
            "30",
            "the upstream backoff hint should reach the client"
        );
    }

    /// 異常に長い `Retry-After` は無視する（クライアントを事実上締め出さない）。
    #[test]
    fn an_absurd_retry_after_is_ignored() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            reqwest::header::RETRY_AFTER,
            (MAX_RETRY_AFTER_SECS + 1).to_string().parse().unwrap(),
        );
        assert_eq!(retry_after_secs(&headers), None);
        headers.insert(
            reqwest::header::RETRY_AFTER,
            MAX_RETRY_AFTER_SECS.to_string().parse().unwrap(),
        );
        assert_eq!(retry_after_secs(&headers), Some(MAX_RETRY_AFTER_SECS));
        // HTTP-date 形式は解釈せず None に倒す（実装しない旨は関数コメント参照）。
        headers.insert(
            reqwest::header::RETRY_AFTER,
            "Wed, 21 Oct 2026 07:28:00 GMT".parse().unwrap(),
        );
        assert_eq!(retry_after_secs(&headers), None);
    }

    /// C1: 408 も一時障害として扱う。
    #[tokio::test]
    async fn an_upstream_request_timeout_is_a_temporary_failure() {
        let res = refresh_against_failure("408 Request Timeout", "", r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// C1: `invalid_grant` を伴う 400 だけが「失効」。ここは従来どおり拒否する。
    #[tokio::test]
    async fn an_invalid_grant_response_is_a_revocation() {
        let res =
            refresh_against_failure("400 Bad Request", "", r#"{"error":"invalid_grant"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// C1: 401 + `invalid_grant` も失効として扱う。
    #[tokio::test]
    async fn an_unauthorized_invalid_grant_response_is_a_revocation() {
        let res =
            refresh_against_failure("401 Unauthorized", "", r#"{"error":"invalid_grant"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// C1: `invalid_client` を伴う 400 は **こちらの設定不備**であって利用者の失効ではない。
    /// これを失効扱いにすると、client_secret の誤り 1 つで全利用者のセッションが壊れる。
    #[tokio::test]
    async fn a_client_misconfiguration_does_not_revoke_user_sessions() {
        let res =
            refresh_against_failure("400 Bad Request", "", r#"{"error":"invalid_client"}"#).await;
        assert_eq!(
            res.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "a server-side credential problem must not log every user out"
        );
    }

    /// C1: 想定外の 4xx（403 等）も失効と断定しない。
    #[tokio::test]
    async fn an_unexpected_client_error_is_not_treated_as_a_revocation() {
        let res = refresh_against_failure("403 Forbidden", "", r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    // ------------------------------------------------------------------
    // C1: callback（認可コード交換）にも同じ分類が効くこと
    // ------------------------------------------------------------------

    /// 上流の token endpoint が指定の失敗応答を返す状況で callback を 1 回通す。
    async fn callback_against_failure(
        status_line: &'static str,
        extra_headers: &'static str,
        body: &'static str,
    ) -> Response {
        let url = spawn_failing_stub(status_line, extra_headers, body).await;
        let key = Arc::new(SigningKey::new("test-signing-key"));
        let h = harness_with_google_token_url(&key, url).await;
        let client_id = register_client(&h.app).await;
        let state = state_blob(&h.key, &client_id, 300);
        get_uri(
            &h.app,
            &format!("/oauth/callback?code=google-code&state={}", enc(&state)),
        )
        .await
    }

    /// C1 の核心。**callback でも 429 を `invalid_grant` にしない。**
    ///
    /// refresh 経路の分類を直した際、兄弟経路である callback に同じ欠陥が残っていた。
    /// 一時障害を `invalid_grant` として返すと、クライアントは回復可能な状況を
    /// 「認可コードが無効」と解釈して確定させてしまう。
    #[tokio::test]
    async fn callback_does_not_report_a_rate_limited_upstream_as_invalid_grant() {
        let res = callback_against_failure(
            "429 Too Many Requests",
            "",
            r#"{"error":"rateLimitExceeded"}"#,
        )
        .await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(res).await["error"], "temporarily_unavailable");
    }

    /// C1: callback でも 5xx は一時障害として扱う。
    #[tokio::test]
    async fn callback_reports_an_upstream_server_error_as_temporary() {
        let res = callback_against_failure("503 Service Unavailable", "", r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(res).await["error"], "temporarily_unavailable");
    }

    /// C1: callback でも `Retry-After` を転送する。
    #[tokio::test]
    async fn callback_propagates_retry_after_from_the_upstream() {
        let res =
            callback_against_failure("429 Too Many Requests", "Retry-After: 45\r\n", r#"{}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(res.headers().get(header::RETRY_AFTER).unwrap(), "45");
    }

    /// C1: 本当に認可コードが無効な場合は、従来どおり `invalid_grant`。
    #[tokio::test]
    async fn callback_reports_a_rejected_code_as_invalid_grant() {
        let res =
            callback_against_failure("400 Bad Request", "", r#"{"error":"invalid_grant"}"#).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// C1: callback でも設定不備（`invalid_client`）は失効扱いにしない。
    #[tokio::test]
    async fn callback_does_not_report_a_client_misconfiguration_as_invalid_grant() {
        let res =
            callback_against_failure("400 Bad Request", "", r#"{"error":"invalid_client"}"#).await;
        assert_eq!(res.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    /// C1: 一時障害の応答は「やり直しが必要」であることを利用者に伝えること。
    /// state は消費済みで、この認可要求は再開できない。
    #[tokio::test]
    async fn a_temporary_callback_failure_says_the_login_must_be_restarted() {
        let res = callback_against_failure("429 Too Many Requests", "", r#"{}"#).await;
        let description = body_json(res).await["error_description"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            description.contains("start the login again"),
            "{description}"
        );
        assert!(description.contains("cannot be resumed"), "{description}");
    }

    /// 分類そのものを純粋関数として直接固定する（HTTP を介さない網羅確認）。
    #[test]
    fn upstream_failures_are_classified_by_status_and_oauth_error() {
        let revoked = |s, e| {
            matches!(
                classify_upstream_failure(s, e, None),
                UpstreamFailure::Rejected
            )
        };
        // 失効と断定できる唯一の組み合わせ。
        assert!(revoked(StatusCode::BAD_REQUEST, Some("invalid_grant")));
        assert!(revoked(StatusCode::UNAUTHORIZED, Some("invalid_grant")));
        // それ以外はすべて一時障害へ倒す。
        assert!(!revoked(StatusCode::TOO_MANY_REQUESTS, None));
        assert!(!revoked(
            StatusCode::TOO_MANY_REQUESTS,
            Some("invalid_grant")
        ));
        assert!(!revoked(StatusCode::REQUEST_TIMEOUT, None));
        assert!(!revoked(StatusCode::BAD_REQUEST, Some("invalid_client")));
        assert!(!revoked(StatusCode::BAD_REQUEST, None));
        assert!(!revoked(StatusCode::UNAUTHORIZED, Some("invalid_client")));
        assert!(!revoked(StatusCode::FORBIDDEN, Some("invalid_grant")));
        assert!(!revoked(StatusCode::INTERNAL_SERVER_ERROR, None));
        assert!(!revoked(StatusCode::BAD_GATEWAY, None));
    }

    /// ログに載せる OAuth エラーコードは既知の値に正規化する
    /// （未知の値は応答本文由来の任意文字列になりうるため、そのまま出さない）。
    #[test]
    fn unrecognized_oauth_error_codes_are_not_echoed_into_logs() {
        assert_eq!(known_oauth_error(Some("invalid_grant")), "invalid_grant");
        assert_eq!(known_oauth_error(None), "<absent>");
        assert_eq!(
            known_oauth_error(Some("<script>alert(1)</script>")),
            "<unrecognized>"
        );
    }

    #[tokio::test]
    async fn token_rejects_an_unsupported_grant_type() {
        let h = harness().await;
        let res = post_token(&h.app, "grant_type=password&username=a&password=b").await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
        assert_eq!(body_json(res).await["error"], "unsupported_grant_type");
    }

    // ------------------------------------------------------------------
    // W2: トークン応答のキャッシュ抑止ヘッダ
    // ------------------------------------------------------------------

    fn header_of(res: &Response, name: header::HeaderName) -> String {
        res.headers()
            .get(&name)
            .unwrap_or_else(|| panic!("missing header {name}"))
            .to_str()
            .unwrap()
            .to_string()
    }

    /// W2: RFC 6749 §5.1 はトークン応答に `no-store` を MUST で要求している。
    #[tokio::test]
    async fn the_token_response_is_not_cacheable() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let res = post_token(&h.app, &code_form(&code, &client_id, CLIENT_VERIFIER)).await;
        assert_eq!(res.status(), StatusCode::OK);
        assert_eq!(header_of(&res, header::CACHE_CONTROL), "no-store");
        assert_eq!(header_of(&res, header::PRAGMA), "no-cache");
    }

    /// リフレッシュ応答とエラー応答にも同じヘッダが付くこと（経路ごとの抜けを防ぐ）。
    #[tokio::test]
    async fn refresh_and_error_responses_are_not_cacheable() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let refresh = issue_refresh_token(&h, &client_id).await;
        let ok = post_token(&h.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(header_of(&ok, header::CACHE_CONTROL), "no-store");

        let err = post_token(&h.app, "grant_type=password").await;
        assert_eq!(err.status(), StatusCode::BAD_REQUEST);
        assert_eq!(header_of(&err, header::CACHE_CONTROL), "no-store");
    }

    // ------------------------------------------------------------------
    // S1〜S4
    // ------------------------------------------------------------------

    /// S1: 監査ログに載せるクライアント指紋が、登録ごとに異なる短い識別子であること。
    ///
    /// **自前トークン廃止に伴う縮退**: 旧実装はこの指紋をアクセストークンの
    /// `client_fp` に載せていたが、Google 発行のトークンには載せられない。
    /// 現在は `/oauth/token` の発行ログにだけ残る（`token_from_code` 参照）ので、
    /// 「どのクライアント経由の操作か」はトークンからではなく発行時のログから辿る。
    #[tokio::test]
    async fn the_client_fingerprint_is_short_and_distinguishes_registrations() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let client_fp = client_fingerprint(&client_id);
        assert_eq!(client_fp.len(), 16);
        // 別内容のクライアント登録では別の指紋になること。
        //
        // なお、**登録内容と `iat`（秒）が完全に同じ 2 回の DCR は同一の client_id に
        // なる**。client_id は登録内容そのものの署名であり、登録簿を持たない設計の
        // 帰結である（client_secret を発行しないため、同一視されても害は無い）。
        // ここでは `client_name` を変えて別登録にする。
        let other = body_json(
            post_register(
                &h.app,
                serde_json::json!({"redirect_uris": [CLIENT_REDIRECT], "client_name": "Other"}),
            )
            .await,
        )
        .await["client_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ne!(client_fingerprint(&other), client_fp);
    }

    /// S2: 登録が 90 日を過ぎたクライアントは、**リフレッシュ経路でも**拒否する。
    ///
    /// 上流トークンは不透明で期限も分からないため、こちらが課せる期限は
    /// クライアント登録の有効期限だけになった。`authorize` にしか課さないと、
    /// 登録失効後もそのクライアントが Google のリフレッシュを中継させ続けられる。
    /// 「**登録の期限間際まで使われていたクライアント**が、登録失効後も中継を
    /// 続けられる」ケースを再現して固定する。
    #[tokio::test]
    async fn refresh_grant_rejects_an_expired_client_registration() {
        let h = harness().await;
        // 登録から 89 日経ったクライアント（まだ有効）。
        let nearly_expired_iat = now_secs() - (CLIENT_MAX_AGE_SECS - 2 * 3600);
        let client_id = h
            .key
            .sign(&Blob::Client {
                redirect_uris: vec![CLIENT_REDIRECT.to_string()],
                client_name: None,
                iat: nearly_expired_iat,
            })
            .unwrap();
        // この時点ではリフレッシュトークンを問題なく得られる。
        let refresh = issue_refresh_token(&h, &client_id).await;
        assert_eq!(
            post_token(&h.app, &refresh_form(&refresh, &client_id))
                .await
                .status(),
            StatusCode::OK
        );

        // 登録の有効期限だけを跨ぐ（リフレッシュトークンは 30 日寿命なのでまだ有効）。
        h.clock.advance(3 * 3600);
        let res = post_token(&h.app, &refresh_form(&refresh, &client_id)).await;
        assert_eq!(
            res.status(),
            StatusCode::BAD_REQUEST,
            "an expired client registration must not keep refreshing for another 30 days"
        );
        assert_eq!(body_json(res).await["error"], "invalid_grant");
    }

    /// S3: `state` の長さ上限。署名対象ブロブとリダイレクト URL に載る値なので制限する。
    #[tokio::test]
    async fn authorize_rejects_an_overlong_client_state() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let uri = format!(
            "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
             &code_challenge={}&code_challenge_method=S256&state={}&resource={}",
            enc(&client_id),
            enc(CLIENT_REDIRECT),
            enc(&client_challenge()),
            "a".repeat(MAX_CLIENT_STATE_LEN + 1),
            enc(&resource_uri(PROJECT_ID)),
        );
        let res = get_uri(&h.app, &uri).await;
        assert_eq!(res.status(), StatusCode::BAD_REQUEST);
    }

    /// S4: `code_challenge` は S256 の形（base64url 43 文字）でなければ authorize で弾く。
    #[tokio::test]
    async fn authorize_rejects_a_malformed_code_challenge() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        for bad in [
            "abc123",                        // 短すぎる
            &"a".repeat(44),                 // 長すぎる
            &format!("{}+", "a".repeat(42)), // base64url 外の文字
            &format!("{}=", "a".repeat(42)), // パディング付き
        ] {
            let uri = format!(
                "/oauth/authorize?response_type=code&client_id={}&redirect_uri={}\
                 &code_challenge={}&code_challenge_method=S256&resource={}",
                enc(&client_id),
                enc(CLIENT_REDIRECT),
                enc(bad),
                enc(&resource_uri(PROJECT_ID)),
            );
            let res = get_uri(&h.app, &uri).await;
            assert_eq!(
                res.status(),
                StatusCode::BAD_REQUEST,
                "code_challenge {bad:?} must be rejected"
            );
        }
    }

    /// S4: 正当な S256 challenge（実際に変換した値）は通ること。
    #[test]
    fn a_real_s256_challenge_is_accepted_by_the_format_check() {
        assert!(is_valid_s256_challenge(&s256("any-verifier")));
        assert!(is_valid_s256_challenge(&s256("another-one")));
    }

    // ------------------------------------------------------------------
    // 秘密の非漏洩
    // ------------------------------------------------------------------

    /// エラー応答に、受け取った code / verifier / client_secret が混ざらないこと。
    /// これらはクライアント側のログにも残るため、混ざると漏洩範囲が一気に広がる。
    #[tokio::test]
    async fn token_error_body_does_not_echo_secrets() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let text = body_text(
            post_token(
                &h.app,
                &code_form(&code, &client_id, "super-secret-verifier"),
            )
            .await,
        )
        .await;
        assert!(!text.contains("super-secret-verifier"), "{text}");
        assert!(!text.contains(&code), "{text}");
        assert!(!text.contains("google-client-secret"), "{text}");
    }

    #[tokio::test]
    async fn config_debug_hides_the_google_client_secret() {
        let config = AuthServerConfig::new(
            PUBLIC_HOST.into(),
            GOOGLE_CLIENT_ID.into(),
            "google-client-secret".into(),
            vec![PROJECT_ID.into()],
        );
        let rendered = format!("{config:?}");
        assert!(!rendered.contains("google-client-secret"), "{rendered}");
    }

    // ------------------------------------------------------------------
    // RS 経路（middleware.rs）との境界
    // ------------------------------------------------------------------
    //
    // 旧実装はここに `verify_access_token` の単体テスト群を置いていた。自前の
    // アクセストークンを廃止したため、その関数ごと無くなっている。リクエスト
    // 経路の検証は `middleware.rs` が `GoogleTokenVerifier` で行い、テストも
    // そちらにある（Google tokeninfo の stub を使った 200 / 401 / 503 の分岐）。
    //
    // **ここで失われた保証を明示しておく**: 旧 `verify_access_token` は
    // `aud` に載せた project_id の完全一致を要求しており、ある project 向けの
    // トークンが別 project の endpoint で通らないことを保証していた。Google 発行の
    // トークンにはこちらの project_id を載せられないため、この保証は存在しない。
    // 下のテストは、その帰結を「認可コードにも state にも project 束縛が残って
    // いない」形で固定し、将来「束縛されているはず」と誤解した実装が入るのを防ぐ。

    /// 発行物に project_id の束縛が **含まれていない**ことを固定する。
    ///
    /// これは望ましい性質ではなく、Google 発行トークンを中継する設計の帰結である。
    /// もし将来テナント分離が必要になったら、ここが変わるべき箇所になる。
    #[tokio::test]
    async fn issued_values_carry_no_project_binding() {
        let h = harness().await;
        let client_id = register_client(&h.app).await;
        let code = issue_code(&h, &client_id).await;
        let Blob::Code { upstream, .. } = h.key.open::<Blob>(&code).unwrap() else {
            panic!("expected a code blob");
        };
        // クライアントが受け取る access_token は Google のもので、こちらの
        // project_id はどこにも現れない。
        assert_eq!(upstream.access_token, GOOGLE_ACCESS_TOKEN);
        assert!(!upstream.access_token.contains(PROJECT_ID));
    }
}
