//! ステートレス OAuth 用の HMAC-SHA256 署名ブロブ。
//!
//! なぜステートレスか:
//! Cloud Run 上の本サービスは `maxScale=1` かつ `minScale` 未設定でゼロスケールする。
//! コンテナはアイドルのたびに停止・再起動するため、発行済みトークン・登録クライアント・
//! 認可コードをプロセスメモリに保持すると、アイドル明けに全部消えて再ログインを強いる。
//! 外部ストア（Firestore / Redis）は追加しない方針のため、**状態は署名付きの値そのものに
//! 埋め込む**。ここはその署名・検証の唯一の経路である。
//!
//! # 鍵はプロセスごとに使い捨て（env / Secret Manager から読まない）
//!
//! 鍵は起動時に OS CSPRNG から生成してメモリにだけ置く（`SigningKey::generate`）。
//! env にも Secret Manager にも鍵は無く、運用者が管理する鍵材料は存在しない。
//!
//! **再起動で鍵が変わることの影響**:
//! - 進行中のログインフロー（`Blob::State` / `Blob::Consent` / `Blob::Code`、
//!   最長 600 秒）は無効になる。利用者はログインをやり直す。
//! - DCR で発行済みのクライアント登録（`Blob::Client`）は無効になる。claude.ai は
//!   接続時に登録をやり直すため、**再登録で自動的に回復する**。
//! - **アクセストークンとリフレッシュトークンは影響を受けない。** これらは Google が
//!   発行した値をそのまま中継しており、この鍵は一切関与しない。したがって
//!   **再起動しても利用者はログアウトしない**。自前トークン発行を廃止したことの
//!   直接の利点がこれであり、鍵を使い捨てにできる根拠でもある。
//!
//! 影響が「最長 600 秒のログインフロー」と「自動回復する DCR 登録」に限られるため、
//! 鍵の寿命・配布・ローテーションを運用作業として抱える価値が無いと判断した。

// `KeyInit` は import しない。`hmac::Mac` と `new` / `new_from_slice` が同名で衝突し、
// 既存の HMAC 側の呼び出しが曖昧になるため、暗号鍵の生成だけ完全修飾で書く。
use chacha20poly1305::aead::rand_core::RngCore;
use chacha20poly1305::aead::{Aead, AeadCore, OsRng};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce};
use hmac::{Hmac, Mac};
use serde::{de::DeserializeOwned, Serialize};
use sha2::Sha256;
use std::fmt;

type HmacSha256 = Hmac<Sha256>;

/// 暗号鍵を署名鍵から派生させるときのドメイン分離文字列。
///
/// **署名鍵と暗号鍵を同一のバイト列にしない**ため、`HMAC-SHA256(secret, INFO)` で
/// 別の鍵を作る（HKDF の expand 段に相当。salt 無しの 1 ブロック展開で 32 バイト＝
/// ChaCha20-Poly1305 の鍵長ちょうどが得られるため、HKDF クレートを足さずに済む）。
/// 同じ秘密を署名と暗号の両方に直接使うと、片方の解析結果がもう片方に波及する。
/// この文字列を変えると既存の封緘ブロブはすべて復号できなくなる（＝実質の鍵ローテーション）。
const BLOB_ENCRYPTION_INFO: &[u8] = b"cs-support-mcp/oauth/blob-encryption/v1";

/// ChaCha20-Poly1305 の nonce 長（96 bit）。
const NONCE_LEN: usize = 12;

/// 署名・検証の失敗分類。
///
/// **Display はブロブ本体・鍵・内部の値を一切含めない。** これらのメッセージは
/// そのままログや HTTP レスポンスに載りうるため、載せた瞬間に有効な認可コードや
/// アクセストークンが Cloud Logging（トークン本体より遥かに広い閲覧母集団と長い
/// 保持期間を持つ）へ流出する。`verifier.rs` の `describe_transport_error` /
/// `sanitize_url` と同じ方針。
#[derive(Debug, PartialEq, Eq)]
pub enum SignError {
    /// `payload.signature` の形になっていない / base64 として不正 / JSON として不正。
    Malformed,
    /// 署名が鍵と一致しない（改竄、または別の鍵で発行された値）。
    BadSignature,
}

impl fmt::Display for SignError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // 「どちらで落ちたか」は運用切り分けに要るが、値そのものは出さない。
            SignError::Malformed => f.write_str("signed value is malformed"),
            SignError::BadSignature => f.write_str("signed value has an invalid signature"),
        }
    }
}

impl std::error::Error for SignError {}

/// HMAC-SHA256 の署名鍵。
///
/// `Debug` を手で実装して鍵を隠す。derive すると、この鍵を含む上位構造体
/// （`AuthServerState` 等）を `?state` でログした瞬間に署名鍵が平文で出る。
pub struct SigningKey {
    secret: Vec<u8>,
    /// 署名鍵から派生した AEAD 鍵（`BLOB_ENCRYPTION_INFO` 参照）。
    /// **秘密**。`Debug` に出さない（下の手書き実装で潰している）。
    cipher: ChaCha20Poly1305,
}

impl fmt::Debug for SigningKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SigningKey(<redacted>)")
    }
}

/// 起動時に生成する署名鍵のバイト数。HMAC-SHA256 のブロック長と同じ 32 バイト
/// （256 bit）で、これ以上長くしても HMAC の強度は上がらない。
const GENERATED_KEY_BYTES: usize = 32;

impl SigningKey {
    /// **本番で使う唯一のコンストラクタ。** OS の CSPRNG から 32 バイトを引いて
    /// プロセス限りの鍵を作る。
    ///
    /// 鍵を env / Secret Manager から読まない理由と、再起動時の影響範囲は
    /// モジュールコメントを参照（要点: アクセストークンは Google 発行なので
    /// 再起動しても利用者はログアウトしない）。
    ///
    /// `OsRng` は `getrandom` 経由で OS のエントロピー源を直接読む。失敗は
    /// OS がエントロピーを供給できない場合のみで、そのまま起動を続けると
    /// **予測可能な鍵で署名する**ことになるため panic させる（fail closed）。
    pub fn generate() -> Self {
        let mut secret = vec![0u8; GENERATED_KEY_BYTES];
        OsRng
            .try_fill_bytes(&mut secret)
            .expect("the OS CSPRNG must be available to generate the OAuth signing key");
        Self::from_bytes(secret)
    }

    /// テスト用。決まった鍵材料から作る（同じ入力なら同じ鍵になり、
    /// 「別プロセスが同じ鍵を持つ」状況を再現できる）。
    #[cfg(test)]
    pub fn new(secret: &str) -> Self {
        Self::from_bytes(secret.as_bytes().to_vec())
    }

    fn from_bytes(secret: Vec<u8>) -> Self {
        let mut mac = HmacSha256::new_from_slice(&secret).expect("HMAC accepts any key length");
        mac.update(BLOB_ENCRYPTION_INFO);
        let derived = mac.finalize().into_bytes();
        Self {
            secret,
            // HMAC-SHA256 の出力は 32 バイトで、ChaCha20-Poly1305 の鍵長と一致する。
            cipher: <ChaCha20Poly1305 as chacha20poly1305::KeyInit>::new(Key::from_slice(&derived)),
        }
    }

    /// payload を JSON 化し、`base64url(json).base64url(hmac)` の形で署名する。
    ///
    /// HMAC は「JSON のバイト列」ではなく「base64url 済みの文字列のバイト列」に対して計算する。
    /// 検証側が再エンコードせずに、受け取った文字列そのものを検証できる形にするため
    /// （JSON の再シリアライズでキー順や空白が変わって署名が壊れる事故を構造的に防ぐ）。
    pub fn sign<T: Serialize>(&self, payload: &T) -> Result<String, serde_json::Error> {
        let json = serde_json::to_vec(payload)?;
        let encoded = b64_encode(&json);
        let mac = self.mac(encoded.as_bytes());
        Ok(format!("{encoded}.{}", b64_encode(&mac)))
    }

    /// 署名を検証して payload を復元する。**有効期限や種別はここでは見ない**
    /// （この層は「値が改竄されていないこと」だけを保証する。期限・種別は呼び出し側が
    /// 型で受けて判定する。責務を分けないと、期限判定を持たない新しいブロブ種別を
    /// 足したときに検証漏れが静かに発生する）。
    pub fn verify<T: DeserializeOwned>(&self, blob: &str) -> Result<T, SignError> {
        let (encoded, signature) = blob.split_once('.').ok_or(SignError::Malformed)?;
        let signature = b64_decode(signature).map_err(|_| SignError::Malformed)?;
        // `verify_slice` は定数時間比較。`==` で比較すると署名の先頭一致長が
        // 応答時間に漏れ、総当たりの手掛かりになる。
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(encoded.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| SignError::BadSignature)?;
        let json = b64_decode(encoded).map_err(|_| SignError::Malformed)?;
        serde_json::from_slice(&json).map_err(|_| SignError::Malformed)
    }

    /// payload を **暗号化した上で**署名する。`sign` と同じ `payload.signature` の形を
    /// 保つが、payload 部が JSON 平文ではなく `nonce || ciphertext` になる。
    ///
    /// なぜ暗号化が要るか:
    /// `sign` のブロブは base64url された JSON 平文であり、**通り道にいる誰でも
    /// 中身を読める**。封緘が要るブロブは 2 系統ある。
    ///
    /// - **認可コード・同意ブロブ**: Google の access_token と refresh_token を
    ///   運ぶ。認可コードはクライアントの redirect_uri へ**クエリ文字列として**渡る
    ///   （ブラウザ履歴・Referer・中間ログに残る）ため、署名だけだと上流
    ///   クレデンシャルがそれらすべてに平文で残る。自前トークン発行を廃止して
    ///   Google のトークンを中継する形にしたことで、この経路の危険度はむしろ上がった。
    /// - **state ブロブ**: Google の authorize URL の `state` クエリに載る。中の
    ///   `google_verifier` は AS が Google に対して使う PKCE verifier で、これが
    ///   読めると「認可コード横取りへの二重防御」の片翼が成立しない。
    ///
    /// nonce は **毎回 OS の CSPRNG（`OsRng` = getrandom）から 96 bit を生成**する。
    /// ChaCha20-Poly1305 は nonce を再利用するとキーストリームが再利用され平文が
    /// 復元可能になるため、カウンタや時刻由来の値は使わない。
    ///
    /// 外側の HMAC は `sign` と共通のまま残す。AEAD 自体が改竄を検出するが、
    /// HMAC を先に定数時間で検証することで、そもそも復号処理に到達させない
    /// （検証順序は `open` を参照）。
    pub fn seal<T: Serialize>(&self, payload: &T) -> Result<String, serde_json::Error> {
        let json = serde_json::to_vec(payload)?;
        let nonce = ChaCha20Poly1305::generate_nonce(&mut OsRng);
        // 失敗は payload が AEAD の最大長（256 GiB 超）を超えた場合のみで、
        // ここで扱うブロブでは到達しない。静かに平文へ倒すより落ちる方が安全。
        let ciphertext = self
            .cipher
            .encrypt(&nonce, json.as_ref())
            .expect("ChaCha20-Poly1305 encryption of a small OAuth blob cannot fail");
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(&nonce);
        sealed.extend_from_slice(&ciphertext);
        let encoded = b64_encode(&sealed);
        let mac = self.mac(encoded.as_bytes());
        Ok(format!("{encoded}.{}", b64_encode(&mac)))
    }

    /// `seal` の逆。署名検証 → 復号 → パースの順で、**どの段で落ちても中身を出さない**。
    ///
    /// `sign` で作られた（暗号化されていない）ブロブをここに持ち込んでも、AEAD の
    /// 認証タグ検証で落ちる。逆に `seal` したブロブを `verify` に持ち込むと、
    /// 復号されない ciphertext は JSON として不正なので `Malformed` で落ちる。
    /// 封緘の有無の取り違えは、どちらの向きでも黙って通らない。
    pub fn open<T: DeserializeOwned>(&self, blob: &str) -> Result<T, SignError> {
        let (encoded, signature) = blob.split_once('.').ok_or(SignError::Malformed)?;
        let signature = b64_decode(signature).map_err(|_| SignError::Malformed)?;
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(encoded.as_bytes());
        mac.verify_slice(&signature)
            .map_err(|_| SignError::BadSignature)?;
        let sealed = b64_decode(encoded).map_err(|_| SignError::Malformed)?;
        // nonce が丸ごと入っていない長さのブロブは、復号を試すまでもなく不正。
        if sealed.len() <= NONCE_LEN {
            return Err(SignError::Malformed);
        }
        let (nonce, ciphertext) = sealed.split_at(NONCE_LEN);
        let json = self
            .cipher
            .decrypt(Nonce::from_slice(nonce), ciphertext)
            // 復号失敗の理由（タグ不一致 / 別鍵 / nonce 破壊）は区別せず、
            // 中身も一切載せない。区別できると攻撃者への oracle になる。
            .map_err(|_| SignError::BadSignature)?;
        serde_json::from_slice(&json).map_err(|_| SignError::Malformed)
    }

    fn mac(&self, message: &[u8]) -> Vec<u8> {
        let mut mac =
            HmacSha256::new_from_slice(&self.secret).expect("HMAC accepts any key length");
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    }
}

/// base64url（パディング無し）。OAuth のパラメータは URL・クエリに載るため、
/// 標準 base64 の `+` `/` `=` を避ける必要がある。
pub fn b64_encode(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

pub fn b64_decode(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    use base64::Engine;
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
    struct Sample {
        who: String,
        n: u64,
    }

    fn sample() -> Sample {
        Sample {
            who: "cs@example.com".to_string(),
            n: 7,
        }
    }

    #[test]
    fn sign_then_verify_roundtrips_the_payload() {
        let key = SigningKey::new("k1");
        let blob = key.sign(&sample()).unwrap();
        let got: Sample = key.verify(&blob).unwrap();
        assert_eq!(got, sample());
    }

    #[test]
    fn verify_rejects_payload_tampering() {
        let key = SigningKey::new("k1");
        let blob = key.sign(&sample()).unwrap();
        let (_, sig) = blob.split_once('.').unwrap();
        let forged = Sample {
            who: "attacker@example.com".to_string(),
            n: 7,
        };
        let tampered = format!(
            "{}.{sig}",
            b64_encode(&serde_json::to_vec(&forged).unwrap())
        );
        assert_eq!(
            key.verify::<Sample>(&tampered).unwrap_err(),
            SignError::BadSignature
        );
    }

    #[test]
    fn verify_rejects_blob_signed_with_another_key() {
        let blob = SigningKey::new("k1").sign(&sample()).unwrap();
        assert_eq!(
            SigningKey::new("k2").verify::<Sample>(&blob).unwrap_err(),
            SignError::BadSignature
        );
    }

    #[test]
    fn verify_rejects_blob_without_signature_separator() {
        let key = SigningKey::new("k1");
        assert_eq!(
            key.verify::<Sample>("no-dot-here").unwrap_err(),
            SignError::Malformed
        );
    }

    #[test]
    fn verify_rejects_non_base64_signature() {
        let key = SigningKey::new("k1");
        let blob = key.sign(&sample()).unwrap();
        let (encoded, _) = blob.split_once('.').unwrap();
        assert_eq!(
            key.verify::<Sample>(&format!("{encoded}.###")).unwrap_err(),
            SignError::Malformed
        );
    }

    /// 署名は正しいが JSON が別の型（別ブロブ種別）のときは Malformed。
    /// 「鍵は正しいので通す」にならないことを固定する。
    #[test]
    fn verify_rejects_valid_signature_over_wrong_shape() {
        let key = SigningKey::new("k1");
        let blob = key.sign(&serde_json::json!({"unrelated": true})).unwrap();
        assert_eq!(
            key.verify::<Sample>(&blob).unwrap_err(),
            SignError::Malformed
        );
    }

    /// 鍵は `Debug` に出さない。上位状態を `?state` でログしたときの漏洩を防ぐ。
    #[test]
    fn debug_of_signing_key_hides_the_secret() {
        let rendered = format!("{:?}", SigningKey::new("super-secret-value"));
        assert!(!rendered.contains("super-secret-value"), "{rendered}");
        assert_eq!(rendered, "SigningKey(<redacted>)");
    }

    // ------------------------------------------------------------------
    // seal / open（C1: Google refresh_token を運ぶブロブの AEAD 封緘）
    // ------------------------------------------------------------------

    const UPSTREAM_SECRET: &str = "1//0gGoogleRefreshTokenMaterial";

    fn secret_sample() -> Sample {
        Sample {
            who: UPSTREAM_SECRET.to_string(),
            n: 7,
        }
    }

    #[test]
    fn seal_then_open_roundtrips_the_payload() {
        let key = SigningKey::new("k1");
        let blob = key.seal(&secret_sample()).unwrap();
        let got: Sample = key.open(&blob).unwrap();
        assert_eq!(got, secret_sample());
    }

    /// C1 の核心。`sign` と違い、封緘したブロブの文字列には上流の秘密が
    /// **部分文字列としても** 現れてはならない。base64url した平文が出ていないこと、
    /// 生の値が出ていないことの両方を押さえる。
    #[test]
    fn sealed_blob_does_not_expose_the_payload_secret() {
        let key = SigningKey::new("k1");
        let sealed = key.seal(&secret_sample()).unwrap();
        assert!(!sealed.contains(UPSTREAM_SECRET), "{sealed}");
        assert!(
            !sealed.contains(&b64_encode(UPSTREAM_SECRET.as_bytes())),
            "{sealed}"
        );
        // 対比: 署名のみの `sign` では実際に平文が読み出せる（この差が封緘の理由）。
        let signed = key.sign(&secret_sample()).unwrap();
        let (encoded, _) = signed.split_once('.').unwrap();
        let recovered = String::from_utf8(b64_decode(encoded).unwrap()).unwrap();
        assert!(recovered.contains(UPSTREAM_SECRET), "{recovered}");
    }

    /// nonce をランダムに引いている以上、同じ payload でも毎回違う ciphertext になる。
    /// 固定 nonce へ退行すると（キーストリーム再利用で平文が復元可能になる）ここで落ちる。
    #[test]
    fn seal_uses_a_fresh_nonce_for_every_call() {
        let key = SigningKey::new("k1");
        let a = key.seal(&secret_sample()).unwrap();
        let b = key.seal(&secret_sample()).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn open_rejects_ciphertext_tampering() {
        let key = SigningKey::new("k1");
        let blob = key.seal(&secret_sample()).unwrap();
        let (encoded, _) = blob.split_once('.').unwrap();
        let mut sealed = b64_decode(encoded).unwrap();
        // nonce の後ろ（ciphertext 本体）を 1 バイト書き換える。
        let last = sealed.len() - 1;
        sealed[last] ^= 0xff;
        let encoded = b64_encode(&sealed);
        // 署名も張り直す。HMAC は通るが AEAD の認証タグで落ちること自体を確認する。
        let forged = format!("{encoded}.{}", b64_encode(&key.mac(encoded.as_bytes())));
        assert_eq!(
            key.open::<Sample>(&forged).unwrap_err(),
            SignError::BadSignature
        );
    }

    #[test]
    fn open_rejects_a_blob_sealed_with_another_key() {
        let blob = SigningKey::new("k1").seal(&secret_sample()).unwrap();
        assert_eq!(
            SigningKey::new("k2").open::<Sample>(&blob).unwrap_err(),
            SignError::BadSignature
        );
    }

    #[test]
    fn open_rejects_a_blob_shorter_than_the_nonce() {
        let key = SigningKey::new("k1");
        let encoded = b64_encode(&[0u8; NONCE_LEN]);
        let blob = format!("{encoded}.{}", b64_encode(&key.mac(encoded.as_bytes())));
        assert_eq!(key.open::<Sample>(&blob).unwrap_err(), SignError::Malformed);
    }

    /// 封緘の有無の取り違えが、どちらの向きでも黙って通らないこと。
    /// `sign` したブロブを `open` に持ち込めば AEAD で落ち、`seal` したブロブを
    /// `verify` に持ち込めば ciphertext が JSON にならず落ちる。
    #[test]
    fn sealed_and_signed_blobs_are_not_interchangeable() {
        let key = SigningKey::new("k1");
        let signed = key.sign(&secret_sample()).unwrap();
        assert_eq!(
            key.open::<Sample>(&signed).unwrap_err(),
            SignError::BadSignature
        );
        let sealed = key.seal(&secret_sample()).unwrap();
        assert_eq!(
            key.verify::<Sample>(&sealed).unwrap_err(),
            SignError::Malformed
        );
    }

    /// 暗号鍵は署名鍵そのものではなく派生値であること。同じ秘密から作った
    /// `SigningKey` 同士は相互運用でき（決定的な派生）、別秘密では復号できない。
    #[test]
    fn encryption_key_is_derived_deterministically_from_the_signing_secret() {
        let blob = SigningKey::new("same-secret")
            .seal(&secret_sample())
            .unwrap();
        let got: Sample = SigningKey::new("same-secret").open(&blob).unwrap();
        assert_eq!(got, secret_sample());
    }

    // ------------------------------------------------------------------
    // generate（プロセス限りの鍵。env / Secret Manager から読まない）
    // ------------------------------------------------------------------

    /// 生成した鍵で署名・封緘の両方が成立すること。`new` と同じ経路を通ることを
    /// 固定し、「本番だけ別の鍵構築経路になる」ずれを防ぐ。
    #[test]
    fn a_generated_key_can_sign_and_seal() {
        let key = SigningKey::generate();
        let signed = key.sign(&sample()).unwrap();
        assert_eq!(key.verify::<Sample>(&signed).unwrap(), sample());
        let sealed = key.seal(&secret_sample()).unwrap();
        assert_eq!(key.open::<Sample>(&sealed).unwrap(), secret_sample());
    }

    /// **これが「再起動すると進行中のログインフローが無効になる」の実体である。**
    /// 別プロセス（= 別の生成鍵）は、前のプロセスが発行した値を一切受理しない。
    /// ここが通ってしまう実装（固定鍵へのフォールバック等）は、鍵を使い捨てに
    /// している前提そのものを崩す。
    #[test]
    fn two_generated_keys_do_not_accept_each_others_blobs() {
        let blob = SigningKey::generate().sign(&sample()).unwrap();
        assert_eq!(
            SigningKey::generate().verify::<Sample>(&blob).unwrap_err(),
            SignError::BadSignature
        );
    }

    /// 生成鍵も `Debug` に出さない。
    #[test]
    fn debug_of_a_generated_key_hides_the_secret() {
        assert_eq!(
            format!("{:?}", SigningKey::generate()),
            "SigningKey(<redacted>)"
        );
    }

    /// エラーメッセージにブロブ本体が混ざらないことを固定する
    /// （そのままログ・HTTP レスポンスに載るため）。
    #[test]
    fn sign_error_display_does_not_leak_the_blob() {
        let key = SigningKey::new("k1");
        let secret_looking = "AAAAsecret-token-material.BBBB";
        let err = key.verify::<Sample>(secret_looking).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("secret-token-material"), "{msg}");
        assert!(!msg.contains("AAAA"), "{msg}");
    }
}
