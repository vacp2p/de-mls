fn main() -> Result<(), std::io::Error> {
    // Link to libwaku only when the "waku" feature is enabled.
    #[cfg(feature = "waku")]
    {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        let libs_dir = std::path::Path::new(&manifest_dir).join("libs");
        println!("cargo:rustc-link-search=native={}", libs_dir.display());

        let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
        match target_os.as_str() {
            "macos" | "linux" => {
                println!("cargo:rustc-link-lib=dylib=waku");
                println!("cargo:rustc-link-arg=-Wl,-rpath,{}", libs_dir.display());
            }
            other => {
                panic!("Unsupported target OS: {other}. Only macOS and Linux are supported.");
            }
        }
    }

    let mut config = prost_build::Config::new();

    config.extern_path(
        ".consensus.v1.Proposal",
        "::hashgraph_like_consensus::protos::consensus::v1::Proposal",
    );
    config.extern_path(
        ".consensus.v1.Vote",
        "::hashgraph_like_consensus::protos::consensus::v1::Vote",
    );

    config.compile_protos(
        &[
            "src/protos/messages/v1/application.proto",
            "src/protos/messages/v1/welcome.proto",
        ],
        &["src/protos/"],
    )?;
    Ok(())
}
