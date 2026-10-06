//! poisoned になった監査ログを検知して、プロセスを再起動させるための終了処理
//! （Issue #62）。
//!
//! 正本は design doc
//! `docs/superpowers/specs/2026-10-06-poisoned-audit-instance-design.md` §2.2・§3.2。
//!
//! `harness::audit::WormAuditLog` 自身はプロセスを終了させない（`std::process::exit` を
//! 呼ばない）。ここは起動処理（`main.rs` / `bin/homesec_advisor.rs`）が通知を待ち受け、
//! 終了コードを決め、grace period を適用するための共通ロジックを置く。
//! `std::process::exit` を呼ぶ箇所自体は起動処理側に残し、ここでは「プロセスを実際に
//! 終了させずに検証できる」部分だけを切り出す（design doc §6 テスト方針）。

use crate::harness::audit::WormAuditLog;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// poisoned 検知による強制終了の終了コード。
///
/// 0 以外であることが外部の再起動ポリシー（Cloud Run の自動再起動、systemd の
/// `Restart=` 等）にとって意味を持つため、個別の分岐ごとに値を決めず固定値として
/// 明示する（design doc §3.2 手順3「0 以外の終了コードでプロセスを終了する」）。
pub const POISONED_EXIT_CODE: i32 = 1;

/// 復旧手順の runbook パス。終了処理のログ出力に含める
/// （design doc §3.2 手順1「再起動後に起動時検証が失敗した場合の参照先」）。
pub const RECOVERY_RUNBOOK_PATH: &str = "docs/runbooks/audit-log-recovery.md";

/// 監視対象の監査ログのいずれかが poisoned になるまで待ち、poisoned になったログの
/// パスを返す。
///
/// 複数の監査ログを同時に監視できる（`homesec_advisor` は advisor 本体用と CS 連携用の
/// 2 つを持つ）。各ログの `wait_poisoned()` を個別タスクで待ち、最初に poisoned に
/// なったログのパスを `mpsc` チャネルで受け取る。
///
/// 呼び出し元が空の `Vec` を渡すと、どの監査ログも poisoned にならないため即座に
/// panic する（`line_adapter` は監査ログを持たないため、この関数自体を呼ばない。
/// 呼び間違いを「一生ハングする」ではなく「即座に失敗する」形で検知するため、
/// ループ終了後に送信側ハンドルを明示的に drop する）。
pub async fn wait_for_any_poisoned(worm_logs: Vec<Arc<WormAuditLog>>) -> PathBuf {
    let (tx, mut rx) = tokio::sync::mpsc::channel::<PathBuf>(1);
    for log in worm_logs {
        let tx = tx.clone();
        tokio::spawn(async move {
            log.wait_poisoned().await;
            // 受信側が既に別の監査ログの通知を受け取って rx を drop していた場合、
            // send は失敗するが無視してよい(他のログが先に poisoned になり、
            // 既に終了処理へ入っている)。
            let _ = tx.send(log.path().to_path_buf()).await;
        });
    }
    drop(tx);
    rx.recv().await.expect(
        "wait_for_any_poisoned was called with an empty Vec, so no audit log can ever poison",
    )
}

/// grace period の経過を待つ（プロセスは終了させない）。
///
/// 呼び出し元（`main.rs` 等）がこの関数の完了を受けて `std::process::exit` を呼ぶ。
/// 実時間の `sleep` を薄く包むだけだが、プロセスを落とさずに「grace period 未経過では
/// 解決しない／経過後に解決する」ことをテストできるようにするために分離する。
///
/// **呼び出し元はこの関数を、poisoned 通知を受け取った**後**に呼ぶこと。** プロセス
/// 起動時点から計測すると、健全に稼働中のサーバが grace period 経過後に強制終了される
/// 事故になる（design doc §3.2 の実装注意）。
pub async fn grace_period_elapsed(grace_period: Duration) {
    tokio::time::sleep(grace_period).await;
}

/// poisoned になるまで待ち、poisoned を示す `tracing::error!` を 1 回出してパスを返す。
///
/// TLS 経路（`axum_server::Handle::graceful_shutdown` が grace period を自前で持つ）と、
/// 平文経路の `poisoned_shutdown_signal` が共有する。ログ文言を 1 箇所にまとめ、
/// 経路ごとの食い違いを防ぐ。
pub async fn wait_and_report_poisoned(
    worm_logs: Vec<Arc<WormAuditLog>>,
    grace_period: Duration,
) -> PathBuf {
    let poisoned_path = wait_for_any_poisoned(worm_logs).await;
    tracing::error!(
        audit_log_path = %poisoned_path.display(),
        grace_period_secs = grace_period.as_secs(),
        runbook = RECOVERY_RUNBOOK_PATH,
        "audit log hash chain is poisoned (a previous append failed to durably \
         persist); shutting down this process so a restart re-runs verify_chain; \
         if verify_chain fails after restart, follow the runbook above"
    );
    poisoned_path
}

/// `axum::serve(..).with_graceful_shutdown(..)` に渡す signal。
///
/// poisoned 通知を受けたら（graceful shutdown の開始として）返る。`axum::serve` 自体には
/// grace period の上限が無いため、**通知を受け取った時点から**別タスクで
/// `grace_period` を計測し、超えたら `on_grace_elapsed` を呼ぶ（プロセス起動時点から
/// 計測しない）。`std::process::exit` はバイナリ側のクロージャが呼ぶ
/// （ライブラリはプロセスを終了させない。design doc §2.2）。
pub async fn poisoned_shutdown_signal<F>(
    worm_logs: Vec<Arc<WormAuditLog>>,
    grace_period: Duration,
    on_grace_elapsed: F,
) where
    F: FnOnce() + Send + 'static,
{
    wait_and_report_poisoned(worm_logs, grace_period).await;
    tokio::spawn(async move {
        grace_period_elapsed(grace_period).await;
        tracing::error!(
            grace_period_secs = grace_period.as_secs(),
            "graceful shutdown exceeded its grace period; forcing process exit now"
        );
        on_grace_elapsed();
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    // ---- poisoned_shutdown_signal ----

    /// grace period は poisoned 通知の受信後から計測する。poisoned 前にどれだけ時間が
    /// 経っても強制終了せず、poison 後 grace period 経過で初めて呼ぶ。
    #[tokio::test(start_paused = true)]
    async fn poisoned_shutdown_signal_measures_grace_period_from_the_notification() {
        let dir = temp_dir();
        let log = Arc::new(WormAuditLog::open(&dir.join("audit.jsonl")).expect("open worm log"));
        let grace_period = Duration::from_secs(30);
        let fired = Arc::new(AtomicBool::new(false));
        let fired_for_callback = fired.clone();

        let signal = tokio::spawn(poisoned_shutdown_signal(
            vec![log.clone()],
            grace_period,
            move || fired_for_callback.store(true, Ordering::SeqCst),
        ));
        tokio::task::yield_now().await;

        // (1) poisoned になる前は、grace period の 10 倍進めても呼ばれない。
        tokio::time::advance(grace_period * 10).await;
        tokio::task::yield_now().await;
        assert!(
            !fired.load(Ordering::SeqCst),
            "must not count the grace period from process start"
        );
        assert!(!signal.is_finished(), "signal must wait for poisoning");

        log.poison_for_test();
        signal.await.expect("signal task must not panic");
        // spawn 済みの grace period タスクが sleep を登録するまで譲る。
        tokio::task::yield_now().await;

        // (2) poison 後 grace_period - 1s では呼ばれない。
        tokio::time::advance(grace_period - Duration::from_secs(1)).await;
        tokio::task::yield_now().await;
        assert!(
            !fired.load(Ordering::SeqCst),
            "must not fire before the grace period elapses after poisoning"
        );

        // (3) さらに 2s 進めると呼ばれる。
        tokio::time::advance(Duration::from_secs(2)).await;
        tokio::task::yield_now().await;
        assert!(
            fired.load(Ordering::SeqCst),
            "must fire once the grace period has elapsed after poisoning"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    fn temp_dir() -> PathBuf {
        std::env::temp_dir().join(format!("shutdown-test-{}", uuid::Uuid::new_v4()))
    }

    // ---- wait_for_any_poisoned ----

    /// 複数の監査ログのうち1つが poisoned になったら、その監査ログのパスで解決する。
    #[tokio::test]
    async fn wait_for_any_poisoned_resolves_with_the_path_of_the_log_that_poisoned() {
        let dir = temp_dir();
        let healthy_path = dir.join("healthy.jsonl");
        let poisoned_path = dir.join("poisoned.jsonl");
        let healthy = Arc::new(WormAuditLog::open(&healthy_path).expect("open healthy log"));
        let poisoned = Arc::new(WormAuditLog::open(&poisoned_path).expect("open poisoned log"));

        let wait_task = tokio::spawn(wait_for_any_poisoned(vec![
            healthy.clone(),
            poisoned.clone(),
        ]));
        // 内部で spawn された監視タスクが `wait_poisoned` の登録を終えるまで一度譲る。
        tokio::task::yield_now().await;
        poisoned.poison_for_test();

        let resolved = tokio::time::timeout(Duration::from_secs(5), wait_task)
            .await
            .expect("wait_for_any_poisoned must resolve once a log poisons")
            .expect("the spawned task must not panic");
        assert_eq!(resolved, poisoned_path);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// すべて健全なままなら、どれだけ待っても解決しない。
    #[tokio::test(start_paused = true)]
    async fn wait_for_any_poisoned_does_not_resolve_while_all_logs_are_healthy() {
        let dir = temp_dir();
        let path = dir.join("audit.jsonl");
        let log = Arc::new(WormAuditLog::open(&path).expect("open worm log"));

        let result =
            tokio::time::timeout(Duration::from_secs(600), wait_for_any_poisoned(vec![log])).await;
        assert!(
            result.is_err(),
            "must not resolve while every watched audit log stays healthy"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    // ---- grace_period_elapsed ----

    /// grace period が経過する前には解決しない。
    #[tokio::test(start_paused = true)]
    async fn grace_period_elapsed_does_not_resolve_before_the_duration() {
        let grace_period = Duration::from_secs(30);
        let result = tokio::time::timeout(
            grace_period - Duration::from_secs(1),
            grace_period_elapsed(grace_period),
        )
        .await;
        assert!(
            result.is_err(),
            "must not resolve before the grace period elapses"
        );
    }

    /// grace period が経過すると解決する。
    #[tokio::test(start_paused = true)]
    async fn grace_period_elapsed_resolves_once_the_duration_elapses() {
        let grace_period = Duration::from_secs(30);
        let result = tokio::time::timeout(
            grace_period + Duration::from_secs(1),
            grace_period_elapsed(grace_period),
        )
        .await;
        assert!(result.is_ok(), "must resolve once the grace period elapses");
    }

    // ---- POISONED_EXIT_CODE ----

    /// 終了コードは 0 以外でなければならない（design doc §3.2 手順3）。
    #[test]
    fn poisoned_exit_code_is_nonzero() {
        assert_ne!(
            POISONED_EXIT_CODE, 0,
            "a poisoned shutdown must use a nonzero exit code so restart policies notice it"
        );
    }

    // ---- 終了処理の統合的な振る舞い: 受付済みリクエストの完了を待つ ----

    /// 通知を受けると graceful shutdown が始まり、受付済みのリクエストが完了してから
    /// サーバが終わる（design doc §3.2 手順2）。実際の TCP リスナー・HTTP リクエストで
    /// 検証する（`std::process::exit` は呼ばない — それは `main.rs` 側の責務）。
    #[tokio::test]
    async fn graceful_shutdown_waits_for_the_in_flight_request_to_finish_after_poisoning() {
        use axum::routing::get;
        use axum::Router;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Mutex as StdMutex;

        let dir = temp_dir();
        let path = dir.join("audit.jsonl");
        let log = Arc::new(WormAuditLog::open(&path).expect("open worm log"));

        let request_completed = Arc::new(AtomicBool::new(false));
        let request_completed_for_handler = request_completed.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel::<()>();
        let started_tx = Arc::new(StdMutex::new(Some(started_tx)));

        let app = Router::new().route(
            "/slow",
            get(move || {
                let request_completed = request_completed_for_handler.clone();
                let started_tx = started_tx.clone();
                async move {
                    if let Some(tx) = started_tx.lock().expect("lock").take() {
                        let _ = tx.send(());
                    }
                    tokio::time::sleep(Duration::from_millis(300)).await;
                    request_completed.store(true, Ordering::SeqCst);
                    "done"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind ephemeral port");
        let addr = listener.local_addr().expect("local addr");

        let shutdown_log = log.clone();
        let shutdown_signal = async move {
            wait_for_any_poisoned(vec![shutdown_log]).await;
        };

        let server = tokio::spawn(async move {
            axum::serve(listener, app)
                .with_graceful_shutdown(shutdown_signal)
                .await
        });

        let client = reqwest::Client::new();
        let url = format!("http://{addr}/slow");
        let request_task = tokio::spawn(async move { client.get(url).send().await });

        tokio::time::timeout(Duration::from_secs(5), started_rx)
            .await
            .expect("handler must start within 5s")
            .expect("start signal must not be dropped");

        // 受付済みリクエストの処理中に、監査ログが poisoned になった想定。
        log.poison_for_test();

        // リクエストはまだ処理中(300ms スリープ中)のはず。この時点でサーバタスクが
        // 終わっていないことを先に確認する(「受付済みの完了を待つ」ことの直接証拠)。
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            !request_completed.load(Ordering::SeqCst),
            "the in-flight request must still be running at this point"
        );
        assert!(
            !server.is_finished(),
            "the server must not shut down before the in-flight request completes"
        );

        let response = request_task
            .await
            .expect("request task must not panic")
            .expect("the in-flight request must complete successfully despite the shutdown");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(request_completed.load(Ordering::SeqCst));

        tokio::time::timeout(Duration::from_secs(5), server)
            .await
            .expect("server must shut down shortly after the in-flight request completes")
            .expect("server task must not panic")
            .expect("serve must return Ok after a graceful shutdown");

        std::fs::remove_dir_all(&dir).ok();
    }
}
