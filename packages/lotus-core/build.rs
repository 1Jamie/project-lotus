use std::process::Command;
use std::env;
use std::path::Path;

fn main() {
    let manifest_dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let script_path = Path::new(&manifest_dir).join("scripts").join("apply-patches.sh");

    println!("cargo:rerun-if-changed=servo-compositor.patch");
    println!("cargo:rerun-if-changed=scripts/apply-patches.sh");

    if script_path.exists() {
        let status = Command::new("bash")
            .arg(script_path)
            .status()
            .expect("Failed to execute apply-patches.sh");
        
        if !status.success() {
            panic!("apply-patches.sh failed with status: {}", status);
        }
    }

    napi_build::setup();
}
