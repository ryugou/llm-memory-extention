fn main() -> Result<(), Box<dyn std::error::Error>> {
    // proto を更新したら必ず build.rs が再実行されて tonic-build が
    // 生成物 (target/.../out/graphrag.rs) を refresh するように rerun-if-changed
    // を明示する。これが無いと proto を編集してもキャッシュ済みの古い
    // 生成コードが使われ続ける。
    println!("cargo:rerun-if-changed=proto/graphrag.proto");
    println!("cargo:rerun-if-changed=build.rs");

    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile_protos(&["proto/graphrag.proto"], &["proto"])?;
    Ok(())
}
