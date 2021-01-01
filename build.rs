fn main() -> std::io::Result<()> {
    prost_build::compile_protos(&["proto/geoip.proto"], &["proto/"])?;
    Ok(())
}