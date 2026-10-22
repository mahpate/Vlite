//! Generates the optional gRPC bindings from `proto/vlite/v1/vlite.proto`.
//! Pure Rust (`protox`): no system `protoc` is needed on any platform.

fn main() {
    println!("cargo:rerun-if-changed=proto");
    #[cfg(feature = "grpc")]
    grpc();
}

#[cfg(feature = "grpc")]
fn grpc() {
    let descriptors = protox::compile(["vlite/v1/vlite.proto"], ["proto"])
        .unwrap_or_else(|error| panic!("invalid proto/vlite/v1/vlite.proto: {error}"));
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(descriptors)
        .unwrap_or_else(|error| panic!("gRPC code generation failed: {error}"));
}
