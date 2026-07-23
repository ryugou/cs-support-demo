/// vegapunk vector 投入まわりの共通ヘルパ。`ingest_urtect` の ManualSection と
/// `ingest_products` の Product の両方が同じ vector 投入契約に従うため、ここに集約する
/// （2026-07-22 Issue #6: KNOWN_MODELS 廃止に伴い ingest_urtect から product embed を
/// 分離した際、embed_all/vector_entry/vector_metadata は挙動を変えずこちらへ移設した）。
use crate::vegapunk::{VectorUpsertEntry, VegapunkClient};
use anyhow::{Context, Result};
use std::collections::HashMap;

/// embed 呼び出しの同時実行数上限。vegapunk backend への負荷配慮のため固定値とする
/// （ingest 規模は ~数十〜百件程度で、動的なチューニングが要る負荷特性ではない）。
pub const EMBED_CONCURRENCY: usize = 4;

/// vector entry の metadata を組み立てる純関数。
///
/// vegapunk `UpsertVectors` の `VectorEntry.metadata` は自由なメタデータ領域ではなく、
/// 固定列へのキー名マッピングである（2026-07-18 backend チーム回答で確定）。認識される
/// キーは `node_id` / `text` / `source_type` / `timestamp_ms` の 4 つのみで、それ以外の
/// キーは backend 側で保存されない。
///
/// 契約: `node_id` は呼び出し側の entry `id`（graph node_id）と同一文字列でなければならない。
/// `GetVectors` / `Search(local)` の schema スコープは `node_id` 列への
/// `starts_with("{schema}:gen{N}:")` で効くため、ここがずれると当該 entry は
/// 全読み出し経路から不可視になる（過去の投入分が見えなかった原因そのもの）。
pub fn vector_metadata(
    node_id: &str,
    text: &str,
    source_type: &str,
    timestamp_ms: &str,
) -> Vec<(String, String)> {
    vec![
        ("node_id".to_string(), node_id.to_string()),
        ("text".to_string(), text.to_string()),
        ("source_type".to_string(), source_type.to_string()),
        ("timestamp_ms".to_string(), timestamp_ms.to_string()),
    ]
}

/// vector entry 1 件 `(id, vector, metadata)` を組み立てる。
///
/// entry の `id`（graph node_id）と `metadata` 内の `node_id` は同一文字列でなければ
/// ならない契約（`vector_metadata` のコメント参照）。呼び出し側が `id` を 2 回書いて
/// 別値が入り込む余地をなくすため、ここで `id` を 1 回だけ受け取り内部で
/// `vector_metadata(&id, ...)` に渡してから entry を返す。
pub fn vector_entry(
    id: String,
    vector: Vec<f32>,
    text: &str,
    source_type: &str,
    timestamp_ms: &str,
) -> VectorUpsertEntry {
    let metadata = vector_metadata(&id, text, source_type, timestamp_ms);
    (id, vector, metadata)
}

/// `items`（`(label, text)`）を最大 `concurrency` 件まで同時実行で embed する。
///
/// - 全タスクを先に spawn するが、各タスクは Semaphore permit を取ってから embed RPC を
///   発行するため、実行中の RPC は常に最大 `concurrency` 件に制限される。
/// - fail-closed: 1 件でも失敗したら失敗 label を context に含めて即座に bail する。
///   early return による `JoinSet` の drop が未完了タスクを abort するため、部分的な
///   ベクトル状態を後続処理に渡さない（呼び出し側は成功時の `Vec` を丸ごと使うか、
///   エラーで ingest 全体を abort するかの二択になる）。
/// - 返り値は `items` と同じ順序を保つ（呼び出し側が id/attrs を zip で組み立てられるように）。
///   completion 順は不定なので、`idx` 付きで結果を集めてから index 順に並べ直す。
pub async fn embed_all(
    client: &VegapunkClient,
    items: Vec<(String, String)>,
    concurrency: usize,
) -> Result<Vec<Vec<f32>>> {
    // concurrency = 0 は permit が永久に取れず全タスクがハングする（プログラミングエラー）。
    // 静かなデッドロックより即時失敗を選ぶ。
    anyhow::ensure!(concurrency > 0, "embed_all: concurrency must be > 0");
    let total = items.len();
    let semaphore = std::sync::Arc::new(tokio::sync::Semaphore::new(concurrency));
    let mut set: tokio::task::JoinSet<(usize, String, Result<Vec<f32>>)> =
        tokio::task::JoinSet::new();
    // JoinError（タスク panic / cancel）経路でも失敗 label を報告できるように、
    // task id -> label を控えておく（成功/embed エラー経路は tuple の label を使う）。
    let mut labels_by_task: HashMap<tokio::task::Id, String> = HashMap::with_capacity(total);

    for (idx, (label, text)) in items.into_iter().enumerate() {
        let client = client.clone();
        let semaphore = semaphore.clone();
        let label_for_join_error = label.clone();
        let handle = set.spawn(async move {
            // owned permit: 同時実行数を concurrency 件に絞る。Semaphore を close() する
            // 経路が無いため acquire_owned が Err になることは実運用上ないが、パニックせず
            // 呼び出し元まで context 付きでエラーを伝搬させる（観測性優先、unwrap しない）。
            let result = match semaphore.acquire_owned().await {
                Ok(_permit) => client.embed(&text).await,
                Err(err) => Err(anyhow::anyhow!("embed concurrency semaphore closed: {err}")),
            };
            (idx, label, result)
        });
        labels_by_task.insert(handle.id(), label_for_join_error);
    }

    let mut results: Vec<(usize, Vec<f32>)> = Vec::with_capacity(total);
    while let Some(joined) = set.join_next().await {
        // JoinError（タスク panic / cancel）自体も fail-closed の対象。
        let (idx, label, result) = joined.map_err(|err| {
            let label = labels_by_task
                .get(&err.id())
                .map(String::as_str)
                .unwrap_or("<unknown item>");
            anyhow::anyhow!(err)
                .context(format!("embed task for {label} panicked or was cancelled"))
        })?;
        let vector = result.with_context(|| format!("embed {label}"))?;
        results.push((idx, vector));
    }

    // 全 join が成功した場合のみここへ到達する（= results は必ず total 件）。
    // idx は enumerate 由来で一意なので、sort 後は入力順と一致する。
    results.sort_by_key(|(idx, _)| *idx);
    Ok(results.into_iter().map(|(_, vector)| vector).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_metadata_node_id_matches_given_id() {
        // node_id は id（graph node_id）と三者一致する契約（2026-07-18 backend 契約）。
        // ここがずれると schema スコープの starts_with フィルタから外れ、全読み出し経路から
        // 不可視になる（今回の修正対象そのもの）。
        let metadata = vector_metadata(
            "urtect:gen1:ManualSection:sec-1",
            "本文",
            "ManualSection",
            "1737200000000",
        );
        let node_id = metadata
            .iter()
            .find(|(k, _)| k == "node_id")
            .map(|(_, v)| v.as_str());
        assert_eq!(node_id, Some("urtect:gen1:ManualSection:sec-1"));
    }

    #[test]
    fn vector_metadata_has_exactly_four_recognized_keys() {
        // backend は固定列マッピングで、認識キー以外は保存されない。旧キー
        // (node_type/section_key/doc_key) を混ぜても無視されるだけの dead weight になるため、
        // 4 キーちょうどであることをテストで固定する。
        // 値も key/value を取り違えていないことを見るため、4 引数それぞれ異なるリテラルにする。
        let metadata = vector_metadata(
            "urtect:gen1:ManualSection:sec-1",
            "抜き差ししてください",
            "ManualSection",
            "1737200000000",
        );
        let mut keys: Vec<&str> = metadata.iter().map(|(k, _)| k.as_str()).collect();
        keys.sort_unstable();
        assert_eq!(keys, vec!["node_id", "source_type", "text", "timestamp_ms"]);

        let get = |key: &str| {
            metadata
                .iter()
                .find(|(k, _)| k == key)
                .map(|(_, v)| v.as_str())
        };
        // text は embed 元テキストそのもの、source_type は呼び出し側が渡した kind 文字列と
        // 一致する契約。キー名だけでなく値も引数と食い違っていないことを固定する。
        assert_eq!(get("text"), Some("抜き差ししてください"));
        assert_eq!(get("source_type"), Some("ManualSection"));
    }

    #[test]
    fn vector_entry_id_matches_metadata_node_id() {
        // entry の id と metadata.node_id が別値になる余地をコンパイル構造で消すための
        // ヘルパ。返る (id, _, metadata) の id と metadata 内 node_id が同一値であることを固定する。
        let (id, vector, metadata) = vector_entry(
            "urtect:gen1:ManualSection:sec-1".to_string(),
            vec![0.1, 0.2],
            "本文",
            "ManualSection",
            "1737200000000",
        );
        let node_id = metadata
            .iter()
            .find(|(k, _)| k == "node_id")
            .map(|(_, v)| v.as_str());
        assert_eq!(id, "urtect:gen1:ManualSection:sec-1");
        assert_eq!(node_id, Some(id.as_str()));
        assert_eq!(vector, vec![0.1, 0.2]);
    }

    #[test]
    fn vector_metadata_timestamp_is_all_digits() {
        // timestamp_ms は時刻フィルタ用の数値文字列という契約。空文字や非数字が混ざると
        // backend 側のフィルタが機能しない。
        let metadata = vector_metadata("id", "text", "ManualSection", "1737200000000");
        let timestamp = metadata
            .iter()
            .find(|(k, _)| k == "timestamp_ms")
            .map(|(_, v)| v.as_str())
            .expect("timestamp_ms key present");
        assert!(!timestamp.is_empty());
        assert!(timestamp.chars().all(|c| c.is_ascii_digit()));
    }
}
