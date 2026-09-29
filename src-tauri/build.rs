fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Permission detection sends `authorizationStatusForMediaType:` to
        // AVCaptureDevice through the Objective-C runtime. Link the owning
        // framework explicitly so the class is guaranteed to be registered.
        println!("cargo:rustc-link-lib=framework=AVFoundation");
    }

    // Only load the canonical capability manifest. Finder/cloud-sync conflict
    // copies such as `desktop 2.json` must never become part of the Tauri ACL.
    println!("cargo:rerun-if-changed=capabilities/desktop.json");
    tauri_build::try_build(
        tauri_build::Attributes::new().capabilities_path_pattern("./capabilities/desktop.json"),
    )
    .expect("failed to build Tauri application");
}
