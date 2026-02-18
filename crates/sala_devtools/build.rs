fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_server(false)
        .build_client(true)
        .compile(
            &["../sala_hud/proto/devcontainer.proto"],
            &["../sala_hud/proto"],
        )?;
    Ok(())
}
