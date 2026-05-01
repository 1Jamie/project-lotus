use std::process::Command;
use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let servo_dir = Path::new(&manifest_dir).join("servo");
    let patch_file = Path::new(&manifest_dir).join("servo-compositor.patch");

    println!("cargo:rerun-if-changed=servo-compositor.patch");
    println!("cargo:rerun-if-changed=scripts/apply-patches.sh"); // Keep this for linux path

    if !servo_dir.exists() {
        eprintln!("[Error] Servo submodule directory not found at {:?}", servo_dir);
        // Do not panic, allow build to continue for now.
        // It might be intended that servo is not always present, or another build step will fetch it.
    } else if !patch_file.exists() {
        eprintln!("[Error] Patch file not found at {:?}", patch_file);
        // Do not panic, allow build to continue for now.
    } else {
        let script_path = Path::new(&manifest_dir).join("scripts").join("apply-patches.js");
        println!("cargo:rerun-if-changed={}", script_path.display());

        if script_path.exists() {
            println!("[Lotus] Ensuring Servo engine patches are applied...");
            let status = Command::new("node")
                .arg(&script_path)
                .status()
                .expect("Failed to execute apply-patches.js");

            if !status.success() {
                // We don't necessarily want to panic here if it's already applied, 
                // but the JS script should exit 0 in that case.
                eprintln!("[Warning] apply-patches.js exited with non-zero status: {}", status);
            }
        } else {
            eprintln!("[Error] apply-patches.js not found at {:?}", script_path);
        }
    }

    napi_build::setup();
}

