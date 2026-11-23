fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        // テスト用の vegapunk 代替（`#[cfg(test)]` の `reply_test_harness`）が同じ proto の
        // サーバ側 trait を実装するために必要。クライアント側の生成物と本番の挙動は変わらない。
        .build_server(true)
        .compile_protos(&["proto/graphrag.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/graphrag.proto");
    Ok(())
}
