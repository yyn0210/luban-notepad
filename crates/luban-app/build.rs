use std::path::PathBuf;

fn main() {
    // rust-i18n embeds the catalogs at compile time but does not register them
    // as Cargo inputs, so make edits to the locale files trigger a rebuild.
    println!("cargo:rerun-if-changed=locales");

    let manifest_dir = PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is not set"),
    );
    let windows_dir = manifest_dir.join("../../packaging/windows");
    let rc_file = windows_dir.join("luban-notepad.rc");

    println!("cargo:rerun-if-changed={}", rc_file.display());
    println!(
        "cargo:rerun-if-changed={}",
        windows_dir.join("luban-notepad.ico").display()
    );

    // GPUI's Windows backend loads icon resource ID 1 from the executable, so
    // embedding it here gives both the taskbar and title-bar icon. On other
    // targets `compile` is a no-op.
    let result = embed_resource::compile(rc_file, embed_resource::ParamsIncludeDirs([windows_dir]));
    match result {
        embed_resource::CompilationResult::Ok | embed_resource::CompilationResult::NotWindows => {}
        other => panic!("failed to embed Windows resources: {other:?}"),
    }
}
