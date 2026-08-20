//! テスト専用の共有ヘルパ。`lib.rs` で `#[cfg(test)]` 付きで宣言するため、この crate の
//! テストバイナリでのみコンパイルされる（本番ビルドには一切含まれない）。
//!
//! ## tracing capture のレース（Issue #31 reviewer 指摘 C1、および flaky 再発の根本対処）
//!
//! 当初の対策は「Dispatch を生成してから破棄するまでの区間を `Mutex` で直列化する」
//! （`with_default` / `set_default` をロックで囲む）ものだった。しかしこれは**同時実行の
//! 防止にしかならない**。`tracing-core` はコールサイトごとの Interest（そのログ行がどの
//! レベルで有効かのキャッシュ）をプロセス全体で一度だけ確定させる。ロックの外側、つまり
//! この capture 機構を一切使わずに同じ warn!/error! コールサイトへ到達するテスト
//! （このモジュールを経由しない他のテスト）が**ロック導入前に**そのコールサイトへ到達して
//! いた場合、Interest は「無効」のまま確定してしまい、以後 `with_default` で subscriber を
//! 差し替えても手遅れになる。ロックは「capture 機構を使うテスト同士」しか直列化できず、
//! 「capture 機構を使わないテストが先に無介入で叩く」ケースを防げない。
//!
//! 対処は方式そのものを変える: **プロセス全体でグローバル subscriber を 1 度だけ
//! インストールする**（Dispatch を差し替えない）。これなら最初の警告ログより前に Interest
//! が「有効」で確定し、以後どのテストがどの順序で叩いても取りこぼされない。ログの受け皿は
//! スレッドローカルバッファにする。Rust の既定テストランナーは 1 テスト = 1 OS スレッドで
//! 実行するため、「対象コード実行前にそのスレッドのバッファをクリアし、実行後に読み出す」
//! だけでテスト間の分離が成立し、`Mutex` による直列化は不要になる。
//!
//! 唯一の前提は「capture 対象のログが、capture を呼んだのと同じ OS スレッド上で出ること」。
//! `#[tokio::test]` は既定で current-thread ランタイムなので、`.await` をまたいでも同一
//! スレッド上で実行される限り成立する。

use std::cell::RefCell;
use std::sync::Once;

thread_local! {
    /// このスレッド上で `install_global_subscriber` インストール後に出た INFO 以上のログ。
    /// `capture_logs` / `capture_logs_async` が実行直前にクリアし、実行直後に読み出す。
    static LOG_BUFFER: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

static INIT: Once = Once::new();

/// スレッドローカルバッファへ書き込むだけの `tracing_subscriber::fmt::MakeWriter` 実装。
#[derive(Clone, Copy, Default)]
struct ThreadLocalWriter;

impl std::io::Write for ThreadLocalWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        LOG_BUFFER.with(|cell| cell.borrow_mut().extend_from_slice(buf));
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl tracing_subscriber::fmt::MakeWriter<'_> for ThreadLocalWriter {
    type Writer = Self;
    fn make_writer(&self) -> Self::Writer {
        *self
    }
}

/// プロセス全体で 1 度だけグローバル subscriber をインストールする。
///
/// レベルフィルタは INFO(2026-08-19、Issue #34 reviewer 指摘: homesec advisor の内部判断
/// ログ `log_turn_decision`(`advisor/api.rs`)が info で出るため、design doc §11 が要求する
/// 「内部判断の info ログ」を `capture_logs` で assert できるよう WARN から引き下げた)。
/// DEBUG を assert するテストが増えたら、ここも合わせて下げること。
fn install_global_subscriber() {
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::INFO)
            .with_ansi(false)
            .with_writer(ThreadLocalWriter)
            .finish();
        tracing::subscriber::set_global_default(subscriber)
            .expect("test log capture subscriber must install exactly once per process");
    });
}

fn take_buffer_text() -> String {
    LOG_BUFFER.with(|cell| {
        let mut buf = cell.borrow_mut();
        let text = String::from_utf8(buf.clone()).expect("tracing fmt writes utf-8");
        buf.clear();
        text
    })
}

/// `f` の実行中にこのスレッドで出た INFO 以上のログを、`f` の戻り値と一緒に返す。
///
/// グローバル subscriber をプロセス全体で 1 度だけインストールしたうえで、実行直前に
/// このスレッドのバッファをクリアする。同じスレッドで直前に他のログが出ていても、その
/// 残骸が今回の capture に混ざることはない。
///
/// 戻り値には INFO ログも混ざる（`install_global_subscriber` 参照）。「WARN / ERROR だけが
/// 欲しい」呼び出し元は、この生ログを [`filter_warn_and_error_lines`] に通すこと
/// （`api::tests::capture_warnings` 等の `capture_warnings` 系ヘルパーがそうしている）。
pub(crate) fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    install_global_subscriber();
    // f() 自身が最初の書き込みになるとは限らないため、実行前に必ずクリアする
    // （このスレッドで過去に発生したログの残骸を持ち越さないため）。
    let _ = take_buffer_text();
    let result = f();
    (result, take_buffer_text())
}

/// [`capture_logs`] の async 版。`#[tokio::test]` の既定（current-thread）ランタイムで
/// `.await` をまたいでも同一スレッド上で実行される限り有効。戻り値に INFO ログが混ざる点も
/// [`capture_logs`] と同じ。
pub(crate) async fn capture_logs_async<Fut, T>(fut: Fut) -> (T, String)
where
    Fut: std::future::Future<Output = T>,
{
    install_global_subscriber();
    let _ = take_buffer_text();
    let result = fut.await;
    (result, take_buffer_text())
}

/// [`capture_logs`] / [`capture_logs_async`] が返す生ログ文字列から、WARN / ERROR の行だけを
/// 残す。tracing の fmt 出力は 1 イベント 1 行なので、行単位のフィルタで足りる。
///
/// 2026-08-19（Issue #34 codex レビュー指摘）: subscriber のレベルフィルタを WARN から INFO へ
/// 引き下げた結果、`capture_warnings` という名前とその `logs.is_empty()` assert の意味が
/// 「WARN が出ていない」から「INFO 以上のログが一切出ていない」へ静かに変わっていた
/// （対象コードへ正常系の info ログを1行足しただけで、無関係な `capture_warnings` テストが
/// 誤ったメッセージで落ちる状態だった）。`api.rs` / `harness::reply` /
/// `harness::product_gate` の `capture_warnings` 系ヘルパーがここへ委譲することで、
/// フィルタの判定条件を3箇所で同一に保つ。
///
/// 2026-08-19（Issue #34 codex レビュー2巡目指摘）: 以前は `line.contains("WARN")` による
/// 部分文字列一致だった。これだと INFO イベントの本文やフィールド値にたまたま `WARN` /
/// `ERROR` という文字列が含まれるだけでその行を誤って残してしまい、`capture_warnings` を
/// 使う正常系テストが `logs.is_empty()` の偽陽性で落ちる余地があった。レベルはログ行の
/// 部分文字列ではなく、行を構成するトークンの1つとして判定する。
///
/// [`install_global_subscriber`] がインストールする subscriber は
/// `tracing_subscriber::fmt()` の既定フォーマット + `.with_ansi(false)` なので、1行は
/// `<RFC3339 timestamp><空白><LEVEL><空白><target>: <message>` の形になる。つまり
/// **空白区切りの2番目のトークンが常にレベル**であるという、このプロセス内の subscriber
/// 設定に固有の前提に依存している。`install_global_subscriber` のフォーマットを変える
/// （例: JSON 出力へ切り替える、`.compact()` 以外のレイアウトにする等）場合は、この関数も
/// 合わせて直すこと。
pub(crate) fn filter_warn_and_error_lines(raw: &str) -> String {
    raw.lines()
        .filter(|line| matches!(line.split_whitespace().nth(1), Some("WARN") | Some("ERROR")))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::filter_warn_and_error_lines;

    /// 実際の WARN 行は残る。
    #[test]
    fn keeps_actual_warn_line() {
        let raw = "2026-08-19T09:00:00.000000Z  WARN cs_support_mcp::advisor: something is off";

        let filtered = filter_warn_and_error_lines(raw);

        assert_eq!(filtered, raw);
    }

    /// 実際の ERROR 行は残る。
    #[test]
    fn keeps_actual_error_line() {
        let raw = "2026-08-19T09:00:00.000000Z ERROR cs_support_mcp::advisor: request failed";

        let filtered = filter_warn_and_error_lines(raw);

        assert_eq!(filtered, raw);
    }

    /// 通常の INFO 行は落ちる。
    #[test]
    fn drops_info_line() {
        let raw = "2026-08-19T09:00:00.000000Z  INFO cs_support_mcp::advisor: turn decided";

        let filtered = filter_warn_and_error_lines(raw);

        assert_eq!(filtered, "");
    }

    /// 本文に "WARN" / "ERROR" という文字列を含む INFO 行は、レベルトークンではなく本文一致
    /// でしかないため落ちる（今回の codex 指摘そのもの）。
    #[test]
    fn drops_info_line_whose_message_body_mentions_warn_and_error() {
        let raw = "2026-08-19T09:00:00.000000Z  INFO cs_support_mcp::advisor: \
                    field=\"WARN threshold exceeded, treat as ERROR\" turn_decided";

        let filtered = filter_warn_and_error_lines(raw);

        assert_eq!(filtered, "");
    }

    /// 複数行の入力から WARN / ERROR 行だけを、元の行順を保って抽出する。
    #[test]
    fn keeps_only_warn_and_error_lines_from_mixed_input() {
        let info_line = "2026-08-19T09:00:00.000000Z  INFO cs_support_mcp::advisor: turn decided";
        let warn_line =
            "2026-08-19T09:00:01.000000Z  WARN cs_support_mcp::advisor: retry scheduled";
        let error_line =
            "2026-08-19T09:00:02.000000Z ERROR cs_support_mcp::advisor: upstream unavailable";
        let raw = [info_line, warn_line, error_line].join("\n");

        let filtered = filter_warn_and_error_lines(&raw);

        assert_eq!(filtered, [warn_line, error_line].join("\n"));
    }
}
