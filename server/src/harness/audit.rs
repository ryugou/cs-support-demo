use crate::harness::scope::AccessScope;
use anyhow::{Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
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
    /// 直近の `append` で `write_all` / `flush` / `sync_all` のいずれかが失敗し、その行が
    /// ディスクに載ったかどうかをこのプロセスから判別できなくなった状態。
    Poisoned,
}

/// 別建て WORM ストア（S1-2 / S1-8 条件 8）。append-only JSONL + hash chain。
/// 削除・更新 API は存在しない。
pub struct WormAuditLog {
    path: PathBuf,
    state: Mutex<(File, ChainState)>, // (append-only file, chain state)
    /// poisoned への遷移を知らせる通知の手段（Issue #62）。値は `poisoned` フラグの
    /// 写しで、**正は `poisoned` フラグ**。遷移時は先にフラグを立て、フラグが
    /// `false` → `true` に変わった 1 回だけ `send_replace(true)` する（「1 回だけ」は値の
    /// 変化で表現する。既に poisoned の状態からの拒否では送らない）。
    ///
    /// `Notify` ではなく `watch` を使う理由: 状態を値として保持するため、受け手が
    /// 遷移の後から待ち始めても（`subscribe_poisoned` の直後に `wait_for` する場合を
    /// 含め）取りこぼさない。起動処理（`main.rs` / `homesec_advisor.rs`）は
    /// `subscribe_poisoned` 経由でこれを監視し、受け取ったらプロセスを終了させる。
    /// `WormAuditLog` 自身は `std::process::exit` を呼ばない（design doc §2.2: 終了を
    /// 決めるのは呼び出し側）。
    poisoned_tx: tokio::sync::watch::Sender<bool>,
    /// poisoned かどうかの正。`state` の `ChainState::Poisoned` と同期して立てる。
    /// `is_poisoned` が `state` のロックを取らずに済むようにするためのもの。`append` は
    /// `sync_all` の間ロックを保持する（GCS FUSE では長時間かかりうる）ため、ヘルス
    /// ハンドラ（async ワーカー上）がロック待ちで tokio ワーカーを塞ぐのを避ける。
    poisoned: std::sync::atomic::AtomicBool,
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
            Ok(existing) => {
                let (hash, line_no) = verify_chain(std::io::BufReader::new(existing))
                    .with_context(|| {
                        format!("audit log {} failed integrity check", path.display())
                    })?;
                // verify_chain は BufRead::lines() で読むため、改行の無い連鎖的に正しい
                // 最終行をそのまま受理してしまう。次の append がその直後に本文を連結すると
                // 1 行に 2 イベントが入った壊れた JSONL になるため、連鎖検証が通った後に
                // 改行の有無を別途検査する（design doc §3.1）。
                verify_trailing_newline(path, line_no)?;
                hash
            }
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
            poisoned_tx: tokio::sync::watch::Sender::new(false),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// この監査ログのファイルパス。終了処理のログ出力（どの監査ログが poisoned に
    /// なったか）に使う。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// ハッシュ連鎖の状態が poisoned かどうか。読み取り専用で状態を変えない。
    ///
    /// 状態を保護する `Mutex` 自体が Rust の意味で poisoned（panic を保持したまま
    /// unlock された）場合も `true` を返す。`ChainState::Poisoned`（耐久化失敗）とは
    /// 原因が異なるが、どちらも「このプロセスはこのログへ以降 append できない」という
    /// 結果は同じため、呼び出し側（ヘルスチェック・終了処理）には同じ扱いで見せる。
    pub fn is_poisoned(&self) -> bool {
        // ロックを取らない（`append` が fsync 中にロックを保持するため）。
        // `Mutex::is_poisoned` も非ブロッキング。
        self.poisoned.load(std::sync::atomic::Ordering::Acquire) || self.state.is_poisoned()
    }

    /// poisoned への遷移を待つための受信側を返す。値が `true` になったら poisoned。
    ///
    /// 受信側は `wait_for(|poisoned| *poisoned)` で待つ。待ち始めた時点で既に `true`
    /// なら即座に返る。複数の受信側を作ってよい。受信側は、この `WormAuditLog` が
    /// drop されると `wait_for` がエラーを返すため、待っている間は `Arc` を保持すること。
    ///
    /// 通知されるのは `append` が検出した遷移（耐久化失敗、`Mutex` の poisoning）だけで、
    /// `is_poisoned()` が `Mutex` の poisoning を `append` の検出前に `true` と答える
    /// 点とは異なる。
    pub fn subscribe_poisoned(&self) -> tokio::sync::watch::Receiver<bool> {
        self.poisoned_tx.subscribe()
    }

    /// poisoned への遷移を記録して知らせる。フラグを立ててから `watch` へ送り、
    /// フラグが `false` → `true` に変わったときだけ送る（2 回目以降は何もしない）。
    fn mark_poisoned(&self) {
        if !self
            .poisoned
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            self.poisoned_tx.send_replace(true);
        }
    }

    /// イベントを追記し event_id を返す。
    ///
    /// **なぜ耐久化（`flush` / `sync_all`）に失敗したら以降の追記を拒否するか
    /// （PR #60 Copilot 指摘の是正）**: `write_all` 自体は成功していても OS バッファに
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
            // Mutex の poisoning は panic の時点では観測できないため、最初に検出した
            // この append で通知する。`mark_poisoned` がハッシュ連鎖の遷移と
            // 「1 回だけ」を共有する（既に通知済みなら再発行しない）。
            self.mark_poisoned();
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
        // 本文と改行を 1 つのバッファにまとめ、write_all で 1 回の書き込みにする
        // （design doc §3.1）。writeln! は本文と改行を別々の書き込みとして発行し得るため、
        // 本文だけが永続化されて改行が失敗する状態が生じ得た。1 回にまとめても短い書き込みや
        // 途中のクラッシュによる窓は消えないため、起動時の末尾改行検証（verify_trailing_newline）
        // で受け止める。
        let mut line = serde_json::to_string(&event)?;
        line.push('\n');
        if let Err(err) = file.write_all(line.as_bytes()) {
            // 部分書き込みの可能性があり、ディスク上の状態を判別できないため poison する。
            *chain = ChainState::Poisoned;
            self.mark_poisoned();
            return Err(err)
                .with_context(|| poisoning_now_error(&self.path))
                .with_context(|| format!("append audit log {}", self.path.display()));
        }
        if let Err(err) = file.flush() {
            *chain = ChainState::Poisoned;
            self.mark_poisoned();
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
            self.mark_poisoned();
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
    ///
    /// `pub(crate)`: `health.rs` / `shutdown.rs` のテストが、実際の I/O 失敗を起こさずに
    /// 「poisoned になった `WormAuditLog`」を用意するために使う（Issue #62）。
    /// 遷移そのもの（`append` の3分岐と同じ文言を使った通知の有無）を固定するテストは、
    /// 既存の実際の書き込み失敗手法（`audit.rs` 内のテスト）を使う。ここは状態遷移と
    /// 通知を実際の分岐と同じように起こす（呼び出し側が `subscribe_poisoned` ベースの
    /// フィクスチャとして使えるようにするため）。
    #[cfg(test)]
    pub(crate) fn poison_for_test(&self) {
        let mut guard = self.state.lock().expect("audit log mutex poisoned");
        guard.1 = ChainState::Poisoned;
        self.mark_poisoned();
        drop(guard);
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

/// 既存ログ全行の hash chain をストリームで検証し、最後の hash と検証済みの行数を返す
/// （空なら genesis hash と行数 0）。1 行でも JSON 不正・チェーン断絶・hash 不一致があれば
/// Err（fail closed）。行数は呼び出し元（`open`）が末尾改行検証のエラー文に使う。
fn verify_chain(reader: impl std::io::BufRead) -> Result<(String, usize)> {
    let mut prev = genesis_hash();
    let mut verified_line_no = 0usize;
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
        verified_line_no = line_no;
    }
    Ok((prev, verified_line_no))
}

/// `verify_chain` が通った既存ファイルに対して、末尾が改行で終わっているかを検査する
/// （design doc §3.1）。`verify_chain` は `BufRead::lines()` で読むため、改行の無い
/// 連鎖的に正しい最終行をそのまま受理してしまう。次の `append` がその直後に本文を連結
/// すると、1 行に 2 イベントが入った壊れた JSONL になるため、改行の有無を最終行が完全か
/// どうかとは別に検査する必要がある。空ファイルは受理する（design doc §3.1 / §6）。
fn verify_trailing_newline(path: &Path, line_no: usize) -> Result<()> {
    let len = std::fs::metadata(path)
        .with_context(|| {
            format!(
                "stat audit log {} for trailing newline check",
                path.display()
            )
        })?
        .len();
    if len == 0 {
        return Ok(());
    }
    let mut tail = File::open(path).with_context(|| {
        format!(
            "reopen audit log {} for trailing newline check",
            path.display()
        )
    })?;
    tail.seek(SeekFrom::End(-1)).with_context(|| {
        format!(
            "seek audit log {} for trailing newline check",
            path.display()
        )
    })?;
    let mut last_byte = [0u8; 1];
    tail.read_exact(&mut last_byte)
        .with_context(|| format!("read audit log {} tail byte", path.display()))?;
    if last_byte[0] != b'\n' {
        anyhow::bail!(
            "audit log {} does not end with a newline after line {line_no}",
            path.display()
        );
    }
    Ok(())
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

    // ---- Issue #62: is_poisoned / poisoned 通知 ----

    /// 健全なログは `is_poisoned` が false を返す。open 直後・append 後のどちらも。
    #[test]
    fn is_poisoned_is_false_when_healthy() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        assert!(
            !log.is_poisoned(),
            "a freshly opened log must not be poisoned"
        );
        log.append(draft("req-1", "allowed")).expect("append");
        assert!(
            !log.is_poisoned(),
            "a successful append must not poison the log"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// `append` が fsync 中に `state` のロックを保持していても、`is_poisoned` は
    /// ブロックせずに返る（ヘルスハンドラが tokio ワーカーを塞がないため）。
    #[test]
    fn is_poisoned_does_not_block_while_another_thread_holds_the_state_lock() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = std::sync::Arc::new(WormAuditLog::open(&path).expect("open worm log"));

        let (locked_tx, locked_rx) = std::sync::mpsc::channel::<()>();
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let holder_log = log.clone();
        let holder = std::thread::spawn(move || {
            let _guard = holder_log.state.lock().expect("lock state");
            locked_tx.send(()).expect("signal locked");
            // テスト側が確認を終えるまでロックを保持する（上限つき）。
            let _ = release_rx.recv_timeout(std::time::Duration::from_secs(10));
        });
        locked_rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("holder thread must take the lock");

        let (result_tx, result_rx) = std::sync::mpsc::channel::<bool>();
        let caller_log = log.clone();
        let caller = std::thread::spawn(move || {
            let _ = result_tx.send(caller_log.is_poisoned());
        });
        let result = result_rx.recv_timeout(std::time::Duration::from_secs(1));

        // 結果に関わらず保持側を解放して、スレッドを残さない。
        release_tx.send(()).ok();
        holder.join().expect("holder thread");
        caller.join().expect("caller thread");

        assert_eq!(
            result,
            Ok(false),
            "is_poisoned must return without waiting for the state lock"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 実際の耐久化失敗（読み取り専用 fd への書き込み、既存テストと同じ手法）で
    /// poisoned に遷移した後は `is_poisoned` が true を返す。
    #[test]
    fn is_poisoned_is_true_after_a_real_durability_failure() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let prev_hash = {
            let log = WormAuditLog::open(&path).expect("open worm log");
            log.append(draft("req-1", "allowed")).expect("append 1");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            v["hash"].as_str().unwrap().to_string()
        };
        let read_only_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open read-only");
        let log = WormAuditLog {
            path: path.clone(),
            state: Mutex::new((read_only_file, ChainState::Verified(prev_hash))),
            poisoned_tx: tokio::sync::watch::Sender::new(false),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        };
        assert!(!log.is_poisoned());
        let _ = log.append(draft("req-2", "escalate"));
        assert!(
            log.is_poisoned(),
            "a real durability failure must flip is_poisoned to true"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// poisoned への遷移（実際の耐久化失敗）で `subscribe_poisoned` の受信側が起こされる。
    /// 複数の waiter を登録しても、全員が同じ単一の遷移で起こされる（health / shutdown の
    /// 両方が同じ `WormAuditLog` を同時に監視できる設計の裏付け）。
    #[tokio::test]
    async fn poisoning_transition_notifies_all_current_waiters_exactly_once() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let prev_hash = {
            let log = WormAuditLog::open(&path).expect("open worm log");
            log.append(draft("req-1", "allowed")).expect("append 1");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            v["hash"].as_str().unwrap().to_string()
        };
        let read_only_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open read-only");
        let log = WormAuditLog {
            path: path.clone(),
            state: Mutex::new((read_only_file, ChainState::Verified(prev_hash))),
            poisoned_tx: tokio::sync::watch::Sender::new(false),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        };

        let mut rx1 = log.subscribe_poisoned();
        let mut rx2 = log.subscribe_poisoned();

        let _ = log.append(draft("req-2", "escalate"));

        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rx1.wait_for(|poisoned| *poisoned),
        )
        .await
        .expect("waiter 1 must be notified by the poisoning transition")
        .expect("sender is alive while the log is");
        tokio::time::timeout(
            std::time::Duration::from_secs(5),
            rx2.wait_for(|poisoned| *poisoned),
        )
        .await
        .expect("waiter 2 must be notified by the same poisoning transition")
        .expect("sender is alive while the log is");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 回帰: 受信側を作った後、一度も待たない（poll しない）うちに poisoned へ遷移しても、
    /// 後から待ち始めた `wait_for` が即座に返る（状態が値として残るため取りこぼさない）。
    #[tokio::test(start_paused = true)]
    async fn waiter_started_after_the_transition_still_sees_poisoned() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let log = WormAuditLog::open(&dir.join("audit.jsonl")).expect("open worm log");
        let mut rx = log.subscribe_poisoned();
        assert!(!log.is_poisoned());
        log.poison_for_test();
        // yield_now() で待機登録を先に成立させない: 遷移の後で初めて待ち始める。
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            rx.wait_for(|poisoned| *poisoned),
        )
        .await
        .expect("a transition before the first wait must not be lost")
        .expect("sender is alive while the log is");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 健全な追記は誰も起こさない（poisoned 専用の通知であり、通常の追記イベントではない）。
    #[tokio::test(start_paused = true)]
    async fn healthy_append_does_not_notify_poisoned_waiters() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open worm log");
        let mut rx = log.subscribe_poisoned();
        log.append(draft("req-1", "allowed")).expect("append");
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.wait_for(|poisoned| *poisoned),
        )
        .await;
        assert!(
            result.is_err(),
            "a healthy append must not notify poisoned waiters"
        );
        drop(result);
        assert!(!rx.has_changed().expect("sender alive"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 既に poisoned な状態からの追記拒否（I/O を一切行わない short-circuit）は、
    /// 遷移時の通知を再発行しない。遷移を観測し終えた受信側で、2 回目の拒否の後に
    /// 新しい変化（`has_changed`）が無いことを確認する。
    #[tokio::test(start_paused = true)]
    async fn short_circuited_reject_on_already_poisoned_log_does_not_notify_again() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let prev_hash = {
            let log = WormAuditLog::open(&path).expect("open worm log");
            log.append(draft("req-1", "allowed")).expect("append 1");
            let body = std::fs::read_to_string(&path).unwrap();
            let v: serde_json::Value = serde_json::from_str(body.lines().last().unwrap()).unwrap();
            v["hash"].as_str().unwrap().to_string()
        };
        let read_only_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open read-only");
        let log = WormAuditLog {
            path: path.clone(),
            state: Mutex::new((read_only_file, ChainState::Verified(prev_hash))),
            poisoned_tx: tokio::sync::watch::Sender::new(false),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        };

        // 1 回目: 実際の耐久化失敗で遷移させる。
        let _ = log.append(draft("req-2", "escalate"));
        assert!(matches!(
            log.state.lock().expect("lock").1,
            ChainState::Poisoned
        ));

        // 遷移を観測し終えた受信側（既読にする）。
        let mut rx = log.subscribe_poisoned();
        rx.wait_for(|poisoned| *poisoned)
            .await
            .expect("sender alive");
        assert!(!rx.has_changed().expect("sender alive"));

        // 2 回目: 既に Poisoned なので I/O 抜きで即座に拒否されるはず。
        let _ = log.append(draft("req-3", "escalate"));

        assert!(
            !rx.has_changed().expect("sender alive"),
            "a reject of an already-poisoned log must not signal a new change"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Mutex が Rust の意味で poisoned（ロック保持中の panic）の場合も、`append` が検出して
    /// 拒否するとき通知を 1 回だけ発行する。2 回目の `append` では再発行しない。
    /// 手段: ロックを保持したまま panic するスレッドを spawn して join する。
    #[tokio::test(start_paused = true)]
    async fn mutex_poisoning_detected_by_append_notifies_exactly_once() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let log = std::sync::Arc::new(
            WormAuditLog::open(&dir.join("audit.jsonl")).expect("open worm log"),
        );
        let holder = log.clone();
        let joined = std::thread::spawn(move || {
            let _guard = holder.state.lock().expect("lock");
            panic!("intentional panic to poison the state mutex");
        })
        .join();
        assert!(joined.is_err(), "the helper thread must have panicked");
        assert!(log.state.is_poisoned());

        // 1 回目の append の前に登録した waiter は、検出時の通知で起きる。
        let mut rx = log.subscribe_poisoned();
        let err = log
            .append(draft("req-1", "allowed"))
            .expect_err("append must be rejected when the state mutex is poisoned");
        assert!(
            format!("{err:#}").contains("state mutex was poisoned"),
            "unexpected error: {err:#}"
        );
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            rx.wait_for(|poisoned| *poisoned),
        )
        .await
        .expect("detecting the mutex poisoning must notify waiters")
        .expect("sender alive");

        // 2 回目: 検出済みなので新しい変化を起こさない。
        assert!(log.append(draft("req-2", "allowed")).is_err());
        assert!(
            !rx.has_changed().expect("sender alive"),
            "a second rejected append must not signal a new change"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// レビュー指摘1: `poison_for_test` は「既に Poisoned な状態からの拒否」だけを固定するテスト
    /// であり、`Verified` → `Poisoned` への**遷移そのもの**は 1 経路もテストしていなかった
    /// （`append` の `sync_all` 失敗分岐から `*chain = ChainState::Poisoned;` を削除しても
    /// `cargo test --lib` は全件 green のまま通ることを変異検証で確認済み）。
    ///
    /// このテストは `poison_for_test` を使わず、読み取り専用でオープンした `File` を使うことで
    /// **実際の I/O 失敗**から `write_all` 分岐の遷移を再現する（読み取り専用 fd への `write`
    /// syscall は EBADF で失敗することを事前に最小のプローブで実測済み）。
    ///
    /// カバーできるのはこの `write_all` 分岐のみ。`std::fs::File` の `Write::flush` は no-op で
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

        // 読み取り専用で開いた File を使い、write_all が実際に失敗する状況を作る
        // (書き込み用に開いていない fd への write syscall は EBADF で拒否される)。
        let read_only_file = OpenOptions::new()
            .read(true)
            .open(&path)
            .expect("open read-only");
        let log = WormAuditLog {
            path: path.clone(),
            state: Mutex::new((read_only_file, ChainState::Verified(prev_hash))),
            poisoned_tx: tokio::sync::watch::Sender::new(false),
            poisoned: std::sync::atomic::AtomicBool::new(false),
        };

        // 1 回目: 実際の write_all 失敗で Err になり、Poisoned へ遷移するはず。
        let err1 = log
            .append(draft("req-2", "escalate"))
            .expect_err("append against a read-only fd must fail");
        let message1 = format!("{err1:#}");
        assert!(
            message1.contains("append audit log"),
            "the failing append must carry the write_all branch's own io context: {message1}"
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
        // 補助的確認: write_all 分岐固有の io context を持たない(=その分岐を再度踏んでいない)。
        assert!(
            !message2.contains("append audit log"),
            "the short-circuited append must not carry the write_all branch's io context \
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

    // ---- Issue #62: 末尾の改行検証 (design doc §3.1) ----

    /// spec (a): 連鎖的に正しい行だけで構成され、末尾に `\n` が無いファイルは `open` が
    /// 拒否する。`verify_chain` は `BufRead::lines()` で読むため、改行の無い連鎖的に
    /// 正しい最終行をそのまま受理してしまう。末尾の改行検証はこれを別に検査する。
    #[test]
    fn open_rejects_a_chain_valid_log_missing_its_trailing_newline() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append 1");
            log.append(draft("req-2", "escalate")).expect("append 2");
        }
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.pop(),
            Some(b'\n'),
            "the file written by append must end with a newline before this test removes it"
        );
        std::fs::write(&path, &bytes).unwrap();

        // `WormAuditLog` は `Debug` を実装していないため `expect_err` は使えない
        // （`Result::err()` は `Ok` 側の型に `Debug` を要求しない）。
        let err = WormAuditLog::open(&path)
            .err()
            .expect("a chain-valid log missing its trailing newline must not open");
        let message = format!("{err:#}");
        assert!(
            message.contains("does not end with a newline after line 2"),
            "unexpected error: {message}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// spec (b): (a) のファイルの末尾に `\n` を 1 バイトだけ追記すれば `open` できる
    /// （本文のバイト列は変えず、欠けていた区切りだけを補う runbook の手順 §5.1-3a の裏付け）。
    #[test]
    fn open_accepts_the_log_once_the_missing_trailing_newline_byte_is_restored() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        {
            let log = WormAuditLog::open(&path).expect("open");
            log.append(draft("req-1", "allowed")).expect("append");
        }
        let mut bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.pop(), Some(b'\n'));
        std::fs::write(&path, &bytes).unwrap();
        assert!(
            WormAuditLog::open(&path).is_err(),
            "sanity: the file without its trailing newline must fail to open first"
        );

        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        WormAuditLog::open(&path)
            .expect("restoring exactly the missing newline byte must allow open to succeed");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// spec (c): 空ファイル（0 バイト）は末尾改行検証の対象外として受理される。
    #[test]
    fn open_accepts_an_empty_audit_log_file() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(&path, b"").unwrap();
        WormAuditLog::open(&path).expect("an empty audit log file must be accepted");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// spec (d): `append` が書く各行は改行で終わる。本文と改行を 1 回の書き込みにまとめた
    /// 後も、1 回の `append` につきファイル上に増える `\n` は常に 1 個であることを確認する。
    #[test]
    fn append_terminates_each_written_line_with_a_newline() {
        let dir = std::env::temp_dir().join(format!("worm-test-{}", uuid::Uuid::new_v4()));
        let path = dir.join("audit.jsonl");
        let log = WormAuditLog::open(&path).expect("open");
        log.append(draft("req-1", "allowed")).expect("append 1");
        log.append(draft("req-2", "escalate")).expect("append 2");

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            bytes.last(),
            Some(&b'\n'),
            "the file must end with a newline after the second append"
        );
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(
            text.matches('\n').count(),
            2,
            "each of the 2 appended lines must be terminated by its own newline"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
