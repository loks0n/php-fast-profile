fn main() -> std::io::Result<()> {
    #[cfg(feature = "pprof")]
    prost_build::compile_protos(&["proto/profile.proto"], &["proto/"])?;
    Ok(())
}
