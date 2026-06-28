fn main() {
    let build_server = std::env::var("CARGO_FEATURE_FAKE_SERVER").is_ok();

    let file_descriptors =
        protox::compile(["proto/headscale/v1/headscale.proto"], ["proto"]).unwrap();

    tonic_prost_build::configure()
        .build_server(build_server)
        .compile_fds(file_descriptors)
        .unwrap();
}
