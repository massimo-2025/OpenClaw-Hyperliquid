fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Only compile proto if the file exists and tonic-build is available
    let proto_path = "proto/signals.proto";
    if std::path::Path::new(proto_path).exists() {
        tonic_build::compile_protos(proto_path)?;
    }
    Ok(())
}
