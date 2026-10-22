fn main() {
    // rust-i18n embeds the catalogs at compile time but does not register them
    // as Cargo inputs, so make edits to the locale files trigger a rebuild.
    println!("cargo:rerun-if-changed=locales");
}
