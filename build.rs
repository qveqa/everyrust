fn main() {
    println!("cargo:rerun-if-changed=assets/logo.ico");
    println!("cargo:rerun-if-changed=assets/windows.rc");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        embed_resource::compile_for("assets/windows.rc", ["everyrust-gui"], embed_resource::NONE)
            .manifest_required()
            .expect("embed EveryRust icon");
    }
}
