#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! Desktop entry point. The application itself lives in the library target so
//! that the OpenHarmony entry point (`crates/luban-ohos`) can reuse it.

fn main() {
    luban_app::run_desktop();
}
