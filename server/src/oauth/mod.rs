pub mod metadata;
pub mod verifier;

/// 認証ミドルウェアが request extensions に注入する、検証済み Google email。
/// 下流の Harness::begin がこれを actor 表に突合する。
#[derive(Debug, Clone)]
pub struct VerifiedEmail(pub String);

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
