fn main() {
    // Only load the canonical capability manifest. Finder/cloud-sync conflict
    // copies such as `desktop 2.json` must never become part of the Tauri ACL.
    println!("cargo:rerun-if-changed=capabilities/desktop.json");
    tauri_build::try_build(
        tauri_build::Attributes::new().capabilities_path_pattern("./capabilities/desktop.json"),
    )
    .expect("failed to build Tauri application");
}
