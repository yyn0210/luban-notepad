fn main() {
    napi_build_ohos::setup();

    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("ohos") {
        // gpui-ohos and the ability bindings are C++ at the ABI boundary; the
        // OHOS linker needs the shared C++ runtime.
        println!("cargo:rustc-link-lib=dylib=c++_shared");
    }
}
