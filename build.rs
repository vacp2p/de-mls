fn main() -> Result<(), std::io::Error> {
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
