fn main() {
    let crate_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    let config = cbindgen::Config::from_file("cbindgen.toml").unwrap_or_default();

    if let Ok(bindings) = cbindgen::Builder::new()
        .with_crate(crate_dir)
        .with_config(config)
        .generate()
    {
        let out_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
        bindings.write_to_file(format!("{}/include/duvm.h", out_dir));
    }
}
