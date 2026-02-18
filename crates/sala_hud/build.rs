fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(feature = "gui")]
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(&["proto/devcontainer.proto"], &["proto"])?;
    Ok(())
}
