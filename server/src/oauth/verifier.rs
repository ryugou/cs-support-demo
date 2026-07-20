use super::AuthError;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

const TOKENINFO_URL: &str = "https://oauth2.googleapis.com/tokeninfo";
/// 検証済み email のキャッシュ上限。Google tokeninfo への RTT を抑えるための短時間キャッシュであり、
/// 失効直後のトークンでも最大この秒数だけ古い判定が使われ得る（許容トレードオフ。トークン単位）。
const MAX_CACHE_TTL: Duration = Duration::from_secs(300);

/// Google tokeninfo のレスポンス（フィールドは Google 仕様通りすべて文字列で返る）。
#[derive(Debug, Deserialize)]
pub(crate) struct TokenInfo {
    pub aud: Option<String>,
    pub azp: Option<String>,
    pub email: Option<String>,
    pub email_verified: Option<String>,
}

/// tokeninfo 応答から検証済み email を導く純粋関数（ネットワーク非依存・単体テスト対象）。
/// aud か azp のどちらかが自クライアント ID と一致し、かつ email_verified=="true" かつ email がある場合のみ Ok。
pub(crate) fn decide(info: &TokenInfo, expected_client_id: &str) -> Result<String, AuthError> {
    let aud_ok = info.aud.as_deref() == Some(expected_client_id)
        || info.azp.as_deref() == Some(expected_client_id);
    if !aud_ok {
        return Err(AuthError::Invalid(
            "token audience does not match this client".into(),
        ));
    }
    if info.email_verified.as_deref() != Some("true") {
        return Err(AuthError::Invalid("email is not verified".into()));
    }
    let email = info
        .email
        .clone()
        .ok_or_else(|| AuthError::Invalid("token has no email claim".into()))?;
    Ok(email)
}

/// Bearer トークンを Google tokeninfo endpoint に照会して検証する。
/// 到達不能・非2xx（400/401除く）・parse失敗は `AuthError::Unreachable` として区別する
/// （運用者が「Google 側の障害/ネットワーク問題」と「トークン自体が無効」を切り分けられるようにするため）。
pub struct GoogleTokenVerifier {
    client_id: String,
    http: reqwest::Client,
    tokeninfo_url: String,
    cache: Mutex<HashMap<String, (String, Instant)>>,
}

impl GoogleTokenVerifier {
    pub fn new(client_id: String) -> Self {
        Self {
            client_id,
            http: reqwest::Client::new(),
            tokeninfo_url: TOKENINFO_URL.to_string(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    /// Bearer を Google tokeninfo で検証し email を返す。トークン単位で最大 5 分キャッシュする。
    pub async fn verify(&self, bearer_token: &str) -> Result<String, AuthError> {
        if let Some(email) = self.cached(bearer_token) {
            return Ok(email);
        }
        let resp = self
            .http
            .get(&self.tokeninfo_url)
            .query(&[("access_token", bearer_token)])
            .send()
            .await
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo request failed: {e}")))?;
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
        let text = resp
            .text()
            .await
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo body read failed: {e}")))?;
        let info: TokenInfo = serde_json::from_str(&text)
            .map_err(|e| AuthError::Unreachable(format!("tokeninfo parse failed: {e}")))?;
        let email = decide(&info, &self.client_id)?;
        self.store(bearer_token, &email);
        Ok(email)
    }

    fn cached(&self, token: &str) -> Option<String> {
        // lock 保持は HashMap 参照のみの短時間。await をまたがないので poison 化のリスクは実質ない。
        let cache = self.cache.lock().unwrap();
        cache
            .get(token)
            .filter(|(_, exp)| *exp > Instant::now())
            .map(|(email, _)| email.clone())
    }

    fn store(&self, token: &str, email: &str) {
        let mut cache = self.cache.lock().unwrap();
        cache.insert(
            token.to_string(),
            (email.to_string(), Instant::now() + MAX_CACHE_TTL),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(aud: &str, verified: &str, email: Option<&str>) -> TokenInfo {
        TokenInfo {
            aud: Some(aud.to_string()),
            azp: None,
            email: email.map(ToString::to_string),
            email_verified: Some(verified.to_string()),
        }
    }

    #[test]
    fn accepts_matching_aud_and_verified_email() {
        let got = decide(
            &info("client-123", "true", Some("a@sivira.co")),
            "client-123",
        )
        .unwrap();
        assert_eq!(got, "a@sivira.co");
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

    #[test]
    fn accepts_when_azp_matches_even_if_aud_differs() {
        let mut i = info("aud-other", "true", Some("a@sivira.co"));
        i.azp = Some("client-123".to_string());
        assert_eq!(decide(&i, "client-123").unwrap(), "a@sivira.co");
    }
}
