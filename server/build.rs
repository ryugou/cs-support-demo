fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .compile_protos(&["proto/graphrag.proto"], &["proto"])?;
    println!("cargo:rerun-if-changed=proto/graphrag.proto");
    Ok(())
}
