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
    /// このスレッド上で `install_global_subscriber` インストール後に出た WARN 以上のログ。
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
/// レベルフィルタは WARN。このリポジトリの capture 系テストは現状すべて WARN 以上しか
/// assert していない（`grep -rn 'with_max_level' server/src` で確認済み）。DEBUG/INFO を
/// assert するテストが増えたら、ここも合わせて下げること。
fn install_global_subscriber() {
    INIT.call_once(|| {
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::WARN)
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

/// `f` の実行中にこのスレッドで出た WARN 以上のログを、`f` の戻り値と一緒に返す。
///
/// グローバル subscriber をプロセス全体で 1 度だけインストールしたうえで、実行直前に
/// このスレッドのバッファをクリアする。同じスレッドで直前に他のログが出ていても、その
/// 残骸が今回の capture に混ざることはない。
pub(crate) fn capture_logs<T>(f: impl FnOnce() -> T) -> (T, String) {
    install_global_subscriber();
    // f() 自身が最初の書き込みになるとは限らないため、実行前に必ずクリアする
    // （このスレッドで過去に発生したログの残骸を持ち越さないため）。
    let _ = take_buffer_text();
    let result = f();
    (result, take_buffer_text())
}

/// [`capture_logs`] の async 版。`#[tokio::test]` の既定（current-thread）ランタイムで
/// `.await` をまたいでも同一スレッド上で実行される限り有効。
pub(crate) async fn capture_logs_async<Fut, T>(fut: Fut) -> (T, String)
where
    Fut: std::future::Future<Output = T>,
{
    install_global_subscriber();
    let _ = take_buffer_text();
    let result = fut.await;
    (result, take_buffer_text())
}
