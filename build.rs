fn main() -> Result<(), std::io::Error> {
    prost_build::compile_protos(
        &[
            "src/protos/messages/v1/welcome.proto",
            "src/protos/messages/v1/application.proto",
            "src/protos/messages/v1/consensus.proto",
        ],
        &["src/protos/"],
    )?;
    Ok(())
}
