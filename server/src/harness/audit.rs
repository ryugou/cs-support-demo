use crate::harness::scope::AccessScope;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 監査イベントの入力（Harness が組み立てる）。
#[derive(Debug, Clone)]
pub struct AuditDraft {
    pub request_id: String,
    pub schema: String,
    /// 監査主体の主識別子（`google-sub:{sub}`）。email 変更に影響されない安定 ID。
    pub actor: String,
    /// 認証時点の email（F4 で追加した加算フィールド）。`actor` を人間が読める形に
    /// 落とすための「当時の値」であり、同一性判定には使わない。
    pub actor_email: String,
    pub used_scope: AccessScope,
    pub retrieved_node_ids: Vec<String>,
    pub decision: String,
    pub route: Option<String>,
    pub governing_norm_ids: Vec<String>,
    /// signal 抽出モード（S1-11 改訂: lexicon_only / hybrid / lexicon_fallback /
    /// not_applicable）。加算フィールド。既存ログ行にはこのキーが無いが、
    /// `verify_chain` は行ごとに実在するキーだけを再ハッシュするため後方互換。
    pub extraction_mode: String,
    /// Issue #58: ヒアリング契約 `product_and_symptom` を宣言した第1層ルール
    /// （`warranty-failure`）の聞き返し判定に Jev の `has_enough_info` を使ったターンのみ
    /// `Some`。使わなかったターンは常に `None`（`extraction_mode` 追加時と同じ加算フィールド。
    /// 既存ログ行にこのキーは無いが、`verify_chain` は行ごとに実在するキーだけを再ハッシュする
    /// ため後方互換）。
    pub jev_has_enough_info: Option<f64>,
}

/// WORM に書かれる 1 行（I5: provenance キー付き構造化レコード）。
#[derive(Debug, Serialize)]
struct AuditEvent<'a> {
    event_id: &'a str,
    timestamp: &'a str,
    request_id: &'a str,
    schema: &'a str,
    /// PunkRecord generation。Step 1 では node_id に gen prefix が含まれるため None。
    generation: Option<i64>,
    actor: &'a str,
    /// F4: 認証時点の email。加算フィールドであり、既存行にこのキーは無いが
    /// `verify_chain` は行ごとに実在するキーだけを再ハッシュするため後方互換
    /// （`extraction_mode` 追加時と同じ性質）。
    actor_email: &'a str,
    used_scope: &'a AccessScope,
    retrieved_node_ids: &'a [String],
    decision: &'a str,
    route: Option<&'a str>,
    governing_norm_ids: &'a [String],
    /// 将来 A / traceable_pairs へ結線するための予約（駆動は後段）。
    graph_provenance_linked: bool,
    /// S1-11 改訂: 今ターンの signal 抽出モード（加算フィールド）。
    extraction_mode: &'a str,
    /// Issue #58: ヒアリング契約を宣言した第1層ルールの聞き返し判定に使った Jev の
    /// `has_enough_info`（加算フィールド）。
    jev_has_enough_info: Option<f64>,
    prev_hash: &'a str,
    hash: &'a str,
}

/// in-memory のチェーン状態。`append` 1 回ごとに `Verified` → `Verified` と進むか、
/// 途中の I/O 失敗で `Poisoned` に落ちて二度と戻らない（`WormAuditLog::open` による
/// 再構築のみが正常状態に戻す手段。詳細は `append` の doc コメント参照）。
enum ChainState {
    /// 直近まで耐久化を確認できた行の hash。次の `append` はこれを prev_hash として使う。
    Verified(String),
    /// 直近の `append` で `writeln!` / `flush` / `sync_all` のいずれかが失敗し、その行が
    /// ディスクに載ったかどうかをこのプロセスから判別できなくなった状態。
    Poisoned,
}

/// 別建て WORM ストア（S1-2 / S1-8 条件 8）。append-only JSONL + hash chain。
/// 削除・更新 API は存在しない。
pub struct WormAuditLog {
    path: PathBuf,
    state: Mutex<(File, ChainState)>, // (append-only file, chain state)
}

impl WormAuditLog {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create audit dir {}", parent.display()))?;
        }
        // 既存ログは全行の hash chain を検証してから継続する（fail closed）。
        // 破損・改ざん・切り詰めを黙って新チェーンで上書きしない。
        // ログは長期運用で巨大化し得るため、一括読み込みでなく 1 行ずつストリーム検証する。
        let prev_hash = match File::open(path) {
            Ok(existing) => verify_chain(std::io::BufReader::new(existing))
                .with_context(|| format!("audit log {} failed integrity check", path.display()))?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => genesis_hash(),
            Err(err) => {
                return Err(err).with_context(|| format!("read audit log {}", path.display()))
            }
        };
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("open audit log {}", path.display()))?;
        Ok(Self {
            path: path.to_path_buf(),
            state: Mutex::new((file, ChainState::Verified(prev_hash))),
        })
    }

    /// イベントを追記し event_id を返す。
    ///
    /// **なぜ耐久化（`flush` / `sync_all`）に失敗したら以降の追記を拒否するか
    /// （PR #60 Copilot 指摘の是正）**: `writeln!` 自体は成功していても OS バッファに
    /// 留まっているだけの可能性があり、後続の `flush` / `sync_all` が失敗した時点では
    /// 「この行が実際にディスク（Cloud Run の gcsfuse マウントなら GCS）まで届いたか」を
    /// このプロセスから判別できない。ここで「届いていない」と楽観して in-memory の
    /// prev_hash を更新しないまま次の `append` を許すと、実際には行が届いていた場合に
    /// 次の行の `prev_hash` がファイル上の直前行の `hash` と食い違い、hash chain がファイル
    /// 上で分岐する。この分岐は書いた瞬間には誰も気づかず、次回起動時の
    /// `WormAuditLog::open`（`verify_chain`）が初めて検知して fail closed で起動を止める
    /// （＝壊れたのは今なのに、気づくのは次のコールドスタート）。
    /// これを避けるため、耐久化に失敗した時点でこのインスタンスを poisoned にし、以降の
    /// `append` を I/O 抜きで即座に拒否する。判別できない状態を推測で継続しない、という
    /// 判断そのものが目的であり、チェーンの自動修復（切り詰め等）は行わない
    /// （改竄検知の意味を失うため）。
    pub fn append(&self, draft: AuditDraft) -> Result<String> {
        let event_id = uuid::Uuid::new_v4().to_string();
        let timestamp = chrono::Utc::now().to_rfc3339();
        let mut guard = self.state.lock().map_err(|_| {
            anyhow::anyhow!(
                "audit log state mutex was poisoned by an earlier panic while holding the lock \
                 (this is Rust Mutex poisoning, not the hash-chain ChainState::Poisoned state)"
            )
        })?;
        let (file, chain) = &mut *guard;
        let prev_hash = match chain {
            ChainState::Verified(hash) => hash.clone(),
            ChainState::Poisoned => return Err(poisoned_error(&self.path)),
        };
        // hash = SHA256(prev_hash + 本文 JSON) — 改竄検知用チェーン
        let payload = serde_json::json!({
            "event_id": event_id,
            "timestamp": timestamp,
            "request_id": draft.request_id,
            "schema": draft.schema,
            "generation": serde_json::Value::Null,
            "actor": draft.actor,
            "actor_email": draft.actor_email,
            "used_scope": draft.used_scope,
            "retrieved_node_ids": draft.retrieved_node_ids,
            "decision": draft.decision,
            "route": draft.route,
            "governing_norm_ids": draft.governing_norm_ids,
            "graph_provenance_linked": false,
            "extraction_mode": draft.extraction_mode,
            "jev_has_enough_info": draft.jev_has_enough_info,
        });
        let payload_text = serde_json::to_string(&payload)?;
        let mut hasher = Sha256::new();
        hasher.update(prev_hash.as_bytes());
        hasher.update(payload_text.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let event = AuditEvent {
            event_id: &event_id,
            timestamp: &timestamp,
            request_id: &draft.request_id,
            schema: &draft.schema,
            generation: None,
            actor: &draft.actor,
            actor_email: &draft.actor_email,
            used_scope: &draft.used_scope,
            retrieved_node_ids: &draft.retrieved_node_ids,
            decision: &draft.decision,
            route: draft.route.as_deref(),
            governing_norm_ids: &draft.governing_norm_ids,
            graph_provenance_linked: false,
            extraction_mode: &draft.extraction_mode,
            jev_has_enough_info: draft.jev_has_enough_info,
            prev_hash: &prev_hash,
            hash: &hash,
        };
        let line = serde_json::to_string(&event)?;
        if let Err(err) = writeln!(file, "{line}") {
            // 部分書き込みの可能性があり、ディスク上の状態を判別できないため poison する。
            *chain = ChainState::Poisoned;
            return Err(err)
                .with_context(|| poisoning_now_error(&self.path))
                .with_context(|| format!("append audit log {}", self.path.display()));
        }
        if let Err(err) = file.flush() {
            *chain = ChainState::Poisoned;
            return Err(err)
                .with_context(|| poisoning_now_error(&self.path))
                .context("flush audit log");
        }
        // Cloud Run では /data を GCS FUSE (gcsfuse) でマウントする運用を想定する。
        // gcsfuse は close/fsync のタイミングで GCS へのアップロードを確定させるため、
        // flush だけではプロセス kill・インスタンス強制終了時にイベントが GCS 側に
        // 届いている保証がない。1 イベントごとに sync_all（fsync 相当）してから
        // event_id を返すことで、「append が成功した」= 「耐久化された」を一致させる
        // （I5: WORM の provenance はイベント単位で耐久していなければ監査の意味がない）。
        // 失敗を握りつぶすと「監査ログに残ったはず」という誤った前提で運用してしまうため、
        // ここも他の I/O と同様に Err を呼び出し元へ伝播する（fail closed）。
        if let Err(err) = file.sync_all() {
            *chain = ChainState::Poisoned;
            return Err(err)
                .with_context(|| poisoning_now_error(&self.path))
                .with_context(|| format!("fsync audit log {}", self.path.display()));
        }
        *chain = ChainState::Verified(hash);
        Ok(event_id)
    }

    /// テスト専用: `flush`/`sync_all` の実失敗を環境非依存に再現するのが難しいため、
    /// poisoned 状態への遷移だけを直接起こす。`#[cfg(test)]` によりテストビルド以外の
    /// バイナリには存在せず、本番コードから呼び出す経路は無い。
    #[cfg(test)]
    fn poison_for_test(&self) {
        let mut guard = self.state.lock().expect("audit log mutex poisoned");
        guard.1 = ChainState::Poisoned;
    }
}

/// この append の write/flush/fsync 失敗そのものに付与する context（レビュー指摘2）。
/// `poisoned_error` とは主語が異なる: こちらは「**この** append が耐久化に失敗した」ことを
/// 述べる（`poisoned_error` は「過去の失敗で既に poisoned な状態からの拒否」を述べる）。
/// この失敗に `poisoned_error` の文言をそのまま貼ると、運用者が存在しない過去の失敗を
/// 探すことになる。
///
/// `append` の 3 つの失敗分岐（write/flush/fsync）はいずれもこの context を**内側**に、
/// io 固有のメッセージ（`"append audit log <path>"` 等）を**外側**に積む。`anyhow::Error` の
/// 非 alternate Display（`{}` / `.to_string()`）は**最外層の context のみ**を出す
/// （`server/src/rmcp_server.rs` の `to_error` は MCP client へ返す際にこれを使う）。
/// したがって最外層は、変更前（コミット `a7a7e0b`）と同一の短く安定した io 固有メッセージ
/// （`"append audit log <path>"` 等）に保つ。仮にこの `poisoning_now_error` の長い定型文を
/// 最外層に置くと、client 可視メッセージが変更前より**悪化**する（短い io 固有メッセージが、
/// この関数が返す約 450 字の poisoning 説明パラグラフに置き換わってしまう）。
/// 逆に言えば、**この順序でも io エラーの root cause（ENOSPC / EIO 等）は client 可視メッセージ
/// には含まれない**。root cause を見るには `{:#}` による alternate Display の全チェーン表示か、
/// `Debug` 表示（`tracing` の `error = ?err` はこちら。`anyhow::Error` の `Debug` は cause chain
/// を出力する）が必要である。
fn poisoning_now_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "this append failed to durably persist, so this process's in-process audit hash-chain \
         state for {} is now unverifiable and every subsequent audit append in this process will \
         be refused. Restart this process to recover: WormAuditLog::open will re-run verify_chain \
         against this file, and if the on-disk chain is broken it will fail closed and refuse to \
         start the service.",
        path.display()
    )
}

/// poisoned な `WormAuditLog` への `append` 拒否時のエラー。運用者が次に何をすべきか
/// （このプロセスは以降書けないこと、再起動すればどう検証されるか）を判断できる情報を含む。
fn poisoned_error(path: &Path) -> anyhow::Error {
    anyhow::anyhow!(
        "audit log {} is poisoned: a previous append failed to durably persist (write/flush/fsync \
         error) and this process can no longer tell whether that row actually reached disk, so \
         further audit appends are refused to avoid guessing and silently breaking the hash \
         chain. Restart this process to recover: WormAuditLog::open will re-run verify_chain \
         against this file, and if the on-disk chain is broken it will fail closed and refuse to \
         start the service.",
        path.display()
    )
}

fn genesis_hash() -> String {
    format!("{:x}", Sha256::digest(b"cs-support-mcp-worm-genesis"))
}

/// 既存ログ全行の hash chain をストリームで検証し、最後の hash を返す（空なら genesis）。
/// 1 行でも JSON 不正・チェーン断絶・hash 不一致があれば Err（fail closed）。
fn verify_chain(reader: impl std::io::BufRead) -> Result<String> {
    let mut prev = genesis_hash();
    for (index, line) in reader.lines().enumerate() {
        let line_no = index + 1;
        let line = line.with_context(|| format!("line {line_no}: read failed"))?;
        let line = line.as_str();
        if line.trim().is_empty() {
            anyhow::bail!("line {line_no}: empty line in append-only log");
        }
        let value: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("line {line_no}: not valid json"))?;
        let serde_json::Value::Object(mut map) = value else {
            anyhow::bail!("line {line_no}: not a json object");
        };
        let line_prev = map
            .remove("prev_hash")
            .and_then(|v| v.as_str().map(ToString::to_string))
            .ok_or_else(|| anyhow::anyhow!("line {line_no}: missing prev_hash"))?;
        let line_hash = map
            .remove("hash")
            .and_then(|v| v.as_str().map(ToString::to_string))
            .ok_or_else(|| anyhow::anyhow!("line {line_no}: missing hash"))?;
        if line_prev != prev {
            anyhow::bail!("line {line_no}: hash chain is broken (prev_hash mismatch)");
        }
        // append 時の payload は serde_json::Value（キーはソート済み）を to_string したもの。
        // 行からも同じ正規形（prev_hash / hash を除いた sorted-key JSON）を再構成して照合する。
        let payload_text = serde_json::to_string(&serde_json::Value::Object(map))?;
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(payload_text.as_bytes());
        let expected = format!("{:x}", hasher.finalize());
        if expected != line_hash {
            anyhow::bail!("line {line_no}: hash mismatch (tampered or corrupted)");
        }
        prev = line_hash;
    }
    Ok(prev)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::scope::AccessScope;

    fn scope() -> AccessScope {
        AccessScope {
            allowed_schemas: vec!["sivira-cs-demo".to_string()],
            max_sensitivity: None,
            label_allowlist: None,
        }
    }

    fn draft(request_id: &str, decision: &str) -> AuditDraft {
        AuditDraft {
            request_id: request_id.to_string(),
            schema: "sivira-cs-demo".to_string(),
            actor: "google-sub:101572111487015263315".to_string(),
            actor_email: "op@sivira.co".to_string(),
            used_scope: scope(),
            retrieved_node_ids: vec!["sivira-cs-demo#gen1/section:doc-1#storage".to_string()],
            decision: decision.to_string(),
            route: None,
            governing_norm_ids: Vec::new(),
            extraction_mode: "not_applicable".to_string(),
            jev_has_enough_info: None,
        }
    }

    #[test]
    fn append_writes_provenance_keyed_jsonl_with_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        let id1 = log.append(draft("req-1", "allowed")).expect("append 1");
        let id2 = log.append(draft("req-2", "escalate")).expect("append 2");
        assert_ne!(id1, id2);

        let body = std::fs::read_to_string(&path).expect("read log");
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is json"))
            .collect();
        assert_eq!(lines.len(), 2);
        // provenance キーが構造化されている（I5、opaque blob でない）
        for line in &lines {
            for key in [
                "event_id",
                "timestamp",
                "request_id",
                "schema",
                "actor",
                "used_scope",
                "retrieved_node_ids",
                "decision",
                "governing_norm_ids",
                "graph_provenance_linked",
                "extraction_mode",
                "actor_email",
                "jev_has_enough_info",
                "prev_hash",
                "hash",
            ] {
                assert!(line.get(key).is_some(), "missing key {key}");
            }
        }
        // hash chain: 2 行目の prev_hash は 1 行目の hash
        assert_eq!(lines[1]["prev_hash"], lines[0]["hash"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Issue #58: `jev_has_enough_info` は Jev 起点の聞き返し判定を使ったターンだけ `Some`
    /// として記録され、JSON では数値としてそのまま読める（`null` ではない）。
    #[test]
    fn append_records_jev_has_enough_info_when_present() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        let mut d = draft("req-jev", "jev_hearing:clarify");
        d.jev_has_enough_info = Some(0.14);
        log.append(d).expect("append");

        let body = std::fs::read_to_string(&path).expect("read log");
        let line: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line["jev_has_enough_info"], serde_json::json!(0.14));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 未使用ターン（`jev_has_enough_info: None`）は JSON 上 `null` として記録され、
    /// かつ hash chain の検証を妨げない（既存の `draft()` ヘルパーが使う既定値の回帰防止）。
    #[test]
    fn append_records_null_for_jev_has_enough_info_when_absent() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        log.append(draft("req-1", "allowed")).expect("append");

        let body = std::fs::read_to_string(&path).expect("read log");
        let line: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert!(line["jev_has_enough_info"].is_null());
        // 再オープンでも整合性検証を通る(このフィールドを追加しても既存の hash chain 検証は
        // 壊れないことの確認)。
        drop(log);
        WormAuditLog::open(&path).expect("log with null jev_has_enough_info must reopen");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// F4: 監査主体は安定した principal ID（`google-sub:{sub}`）で記録し、
    /// email は「当時の値」として別フィールド `actor_email` に残す。
    #[test]
    fn append_records_stable_actor_id_and_point_in_time_email_separately() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        log.append(draft("req-1", "allowed")).expect("append");

        let body = std::fs::read_to_string(&path).expect("read log");
        let line: serde_json::Value = serde_json::from_str(body.lines().next().unwrap()).unwrap();
        assert_eq!(line["actor"], "google-sub:101572111487015263315");
        assert_eq!(line["actor_email"], "op@sivira.co");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 旧形式エントリ（`actor` が `google:{email}`、`actor_email` キーそのものが無い）を
    /// 含むログを手で組み立てる。`actor_email` 追加前に書かれた本番エントリの再現。
    fn write_legacy_entry(path: &Path) -> String {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let payload = serde_json::json!({
            "event_id": "legacy-event-1",
            "timestamp": "2026-07-01T00:00:00+00:00",
            "request_id": "req-legacy",
            "schema": "sivira-cs-demo",
            "generation": serde_json::Value::Null,
            "actor": "google:alice@sivira.co",
            "used_scope": scope(),
            "retrieved_node_ids": Vec::<String>::new(),
            "decision": "allowed",
            "route": serde_json::Value::Null,
            "governing_norm_ids": Vec::<String>::new(),
            "graph_provenance_linked": false,
            "extraction_mode": "not_applicable",
        });
        let payload_text = serde_json::to_string(&payload).unwrap();
        let prev = genesis_hash();
        let mut hasher = Sha256::new();
        hasher.update(prev.as_bytes());
        hasher.update(payload_text.as_bytes());
        let hash = format!("{:x}", hasher.finalize());
        let mut map = payload.as_object().unwrap().clone();
        map.insert("prev_hash".to_string(), serde_json::json!(prev));
        map.insert("hash".to_string(), serde_json::json!(hash));
        let line = serde_json::to_string(&serde_json::Value::Object(map)).unwrap();
        std::fs::write(path, format!("{line}\n")).unwrap();
        hash
    }

    /// F4 後方互換: `actor_email` の追加は加算フィールドであり、既存エントリを壊さない。
    /// `verify_chain` は行ごとに実在するキーだけを再ハッシュするため、
    /// 旧エントリ（`actor_email` 無し）を含むログも整合性検証を通り、チェーンが継続する。
    #[test]
    fn legacy_entries_without_actor_email_still_verify_and_chain_continues() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let legacy_hash = write_legacy_entry(&path);

        // 旧エントリを含むログを開けること（fail closed 検証を通過する）
        let log = WormAuditLog::open(&path).expect("legacy log must still open");
        log.append(draft("req-new", "allowed")).expect("append");

        let body = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // 旧エントリはそのまま読める
        assert_eq!(lines[0]["actor"], "google:alice@sivira.co");
        assert!(lines[0].get("actor_email").is_none());
        // 新エントリはチェーンを継続する
        assert_eq!(lines[1]["prev_hash"].as_str().unwrap(), legacy_hash);
        // 形式は prefix で判別できる（旧 = google:{email} / 新 = google-sub:{sub}）
        assert!(!lines[0]["actor"]
            .as_str()
            .unwrap()
            .starts_with("google-sub:"));
        assert!(lines[1]["actor"]
            .as_str()
            .unwrap()
            .starts_with("google-sub:"));
        // 旧エントリの email は actor 文字列から、新エントリは actor_email から辿れる
        // （＝ cutover をまたいだ同一人物の追跡経路が残っている）
        assert!(lines[0]["actor"].as_str().unwrap().contains('@'));
        assert_eq!(lines[1]["actor_email"], "op@sivira.co");

        // 再オープンしても整合性検証を通る
        drop(log);
        WormAuditLog::open(&path).expect("mixed-format log must reopen");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rejects_tampered_log() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append 1");
            log.append(draft("req-2", "escalate")).expect("append 2");
        }
        // 1 行目の decision を書き換える（改ざん）→ 再オープンは失敗する
        let body = std::fs::read_to_string(&path).unwrap();
        let tampered = body.replacen("\"decision\":\"allowed\"", "\"decision\":\"denied!\"", 1);
        assert_ne!(body, tampered, "tamper must change the file");
        std::fs::write(&path, tampered).unwrap();
        assert!(WormAuditLog::open(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn open_rejects_corrupted_tail() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
        }
        // 途中で切れた行（クラッシュ・破損相当）→ genesis に黙って戻らず失敗する
        let mut body = std::fs::read_to_string(&path).unwrap();
        body.push_str("{\"broken\":");
        std::fs::write(&path, body).unwrap();
        assert!(WormAuditLog::open(&path).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// レビュー指摘1: `poison_for_test` は「既に Poisoned な状態からの拒否」だけを固定するテスト
    /// であり、`Verified` → `Poisoned` への**遷移そのもの**は 1 経路もテストしていなかった
    /// （`append` の `sync_all` 失敗分岐から `*chain = ChainState::Poisoned;` を削除しても
    /// `cargo test --lib` は全件 green のまま通ることを変異検証で確認済み）。
    ///
    /// このテストは `poison_for_test` を使わず、読み取り専用でオープンした `File` を使うことで
    /// **実際の I/O 失敗**から `writeln!` 分岐の遷移を再現する（読み取り専用 fd への `write`
    /// syscall は EBADF で失敗することを事前に最小のプローブで実測済み）。
    ///
    /// カバーできるのはこの `writeln!` 分岐のみ。`std::fs::File` の `Write::flush` は no-op で
    /// 常に `Ok` を返すため到達不能であり、`sync_all` の失敗を移植性のある形で起こす手段が無い
    /// （実測でも読み取り専用 fd に対して `flush` / `sync_all` はどちらも `Ok` を返した）。
    /// 残り 2 分岐（`flush` / `sync_all` 失敗時の poison 遷移）は、3 分岐とも同一の
    /// `*chain = ChainState::Poisoned; return Err(...)` パターンを踏んでいることをコード
    /// レビューで担保する。
    ///
    /// codex レビュー指摘2の是正: 「2 回目の append が short-circuit した」ことを、内部状態
    /// （`ChainState::Poisoned`）と `poisoned_error()` 固有の肯定的な文言の 2 つで固定する。
    /// 以前は否定形の文言一致（`!message2.contains("append audit log")`）だけで判定しており、
    /// エラー文言を変えるとテストの意味が壊れる上、読み取り専用ファイルなので仮に 2 回目が
    /// 再度 I/O を試みても行数は変わらず「I/O を一切行わなかった」ことの証明にならなかった。
    #[test]
    fn append_poisons_on_actual_write_failure_and_then_short_circuits() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");

        // 既存行があるチェーンから始める(通常の open + append で 1 行作る)。
        let prev_hash = {
            let log = WormAuditLog::open(&path).expect("open worm log");
            log.append(draft("req-1", "allowed")).expect("append 1");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            v["hash"].as_str().unwrap().to_string()
        };
        let lines_before = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines_before, 1);

        // 読み取り専用で開いた File を使い、writeln! が実際に失敗する状況を作る
        // (書き込み用に開いていない fd への write syscall は EBADF で拒否される)。
        let read_only_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open read-only");
        let log = WormAuditLog {
            path: path.clone(),
            state: Mutex::new((read_only_file, ChainState::Verified(prev_hash))),
        };

        // 1 回目: 実際の writeln! 失敗で Err になり、Poisoned へ遷移するはず。
        let err1 = log
            .append(draft("req-2", "escalate"))
            .expect_err("append against a read-only fd must fail");
        let message1 = format!("{err1:#}");
        assert!(
            message1.contains("append audit log"),
            "the failing append must carry the writeln! branch's own io context: {message1}"
        );
        // 状態そのもの（ChainState::Poisoned への遷移）を直接確認する。文言に依存しない。
        assert!(
            matches!(log.state.lock().expect("lock").1, ChainState::Poisoned),
            "an append that failed to durably persist must transition the chain state to Poisoned"
        );

        // 2 回目: 既に Poisoned のはずなので、I/O を一切行わず即座に拒否される。
        // `poisoned_error()` 固有の文言（肯定形）で、拒否の理由が「拒否契約」側であることを
        // 確認する。行数不変 assert と合わせて「I/O を一切行わずこの契約で拒否した」ことを示す。
        let err2 = log
            .append(draft("req-3", "escalate"))
            .expect_err("poisoned log must refuse further appends without touching the file");
        let message2 = format!("{err2:#}");
        assert!(
            message2.contains("a previous append failed to durably persist"),
            "the short-circuited append must be rejected via poisoned_error()'s own wording: {message2}"
        );
        // 補助的確認: writeln! 分岐固有の io context を持たない(=その分岐を再度踏んでいない)。
        assert!(
            !message2.contains("append audit log"),
            "the short-circuited append must not carry the writeln! branch's io context \
             (that would mean it attempted I/O again instead of refusing early): {message2}"
        );

        let lines_after = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(
            lines_before, lines_after,
            "neither append succeeded, so the file must be unchanged"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// PR #60 Copilot 指摘: `flush`/`sync_all` が失敗すると、行はファイルに載ったかもしれないのに
    /// in-memory の prev_hash が古いままになり、次の append が古い prev_hash で連鎖してファイル上の
    /// チェーンを壊す（次回起動の `verify_chain` が fail closed で拒否し、サービスが起動しなくなる）。
    /// これを防ぐため、耐久化に失敗したら以降の append を全て拒否する（poisoned）。
    /// 実際に `flush`/`sync_all` を失敗させるのは環境非依存に再現できないため、
    /// テスト専用 API `poison_for_test`（`#[cfg(test)]`、本番ビルドには存在しない）で
    /// 同じ状態遷移を直接再現する。
    #[test]
    fn append_after_poisoned_refuses_write_and_leaves_file_untouched() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open");
        log.append(draft("req-1", "allowed")).expect("append 1");
        let lines_before = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(lines_before, 1);

        log.poison_for_test();

        let err = log
            .append(draft("req-2", "escalate"))
            .expect_err("poisoned log must refuse further appends");
        // 運用者が次のアクションを判断できる情報（ファイルパス・再起動時の挙動）を含む。
        let message = format!("{err:#}");
        assert!(
            message.contains(&path.display().to_string()),
            "error must include the audit log file path: {message}"
        );
        assert!(
            message.contains("verify_chain"),
            "error must explain that restart re-runs verify_chain: {message}"
        );

        let lines_after = std::fs::read_to_string(&path).unwrap().lines().count();
        assert_eq!(
            lines_before, lines_after,
            "a poisoned append must not perform any I/O against the WORM file"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn reopen_continues_hash_chain() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let first_hash;
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            first_hash = v["hash"].as_str().unwrap().to_string();
        }
        // 再オープン（プロセス再起動相当）でもチェーンが繋がる
        let log = WormAuditLog::open(&path).expect("reopen");
        log.append(draft("req-2", "escalate")).expect("append");
        let body = std::fs::read_to_string(&path).unwrap();
        let last: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
        assert_eq!(last["prev_hash"].as_str().unwrap(), first_hash);
        std::fs::remove_dir_all(&dir).ok();
    }
}
