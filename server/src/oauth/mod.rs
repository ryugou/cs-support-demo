pub mod authserver;
pub mod metadata;
pub mod middleware;
pub mod signing;
pub mod verifier;

/// 認証ミドルウェアが request extensions に注入する、検証済み Google identity。
/// 下流の `Harness::begin` がこれを actor に写す。
///
/// 2026-07 改訂 / codex adversarial-review F4:
/// 旧 `VerifiedEmail(String)` は email だけを運んでおり、actor ID も `google:{email}` から
/// 生成して WORM 監査に記録していた。email は変更・別名・再割当てがありうるため、
/// 監査主体としては不安定である（同一人物が別 actor に見え、別人が同一 actor に見えうる）。
/// 現行は tokeninfo の `sub`（Google の安定した principal ID）を主識別子として運び、
/// email は「認証時点の値」として併走させる。
#[derive(Debug, Clone)]
pub struct VerifiedIdentity {
    /// Google tokeninfo の `sub`。principal ごとに安定・不変で、再割当てされない。
    /// actor ID と WORM 監査の主識別子はこれを使う。
    pub sub: String,
    /// 認証時点の検証済み email。表示・調査・cutover をまたいだ名寄せ用の
    /// 「当時の値」であり、主識別子にはしない。
    pub email: String,
}

/// Google トークン検証の失敗分類。ミドルウェアが HTTP ステータスへ写像する。
#[derive(Debug)]
pub enum AuthError {
    /// Bearer が無い → 401 + WWW-Authenticate
    Missing,
    /// 検証失敗（無効/期限切れ/aud不一致/email未検証）→ 401 + WWW-Authenticate
    Invalid(String),
    /// Google 到達不能 → 503
    Unreachable(String),
}
