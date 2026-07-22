fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_prost_build::configure()
        .build_server(true)
        .build_client(true)
        .compile_protos(
            &[
                "proto/file_service.proto",
                "proto/migrate_service.proto",
                "proto/pair_service.proto",
                "proto/stream_service.proto",
            ],
            &["proto"],
        )?;
    Ok(())
}
