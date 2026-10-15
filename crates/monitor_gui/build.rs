use std::path::Path;

fn main() {
    let module = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../priv/juanz/mod.rs");

    println!("cargo:rerun-if-changed={}", module.display());

    if module.is_file() {
        println!("cargo:rustc-cfg=juanz");
    }
}
