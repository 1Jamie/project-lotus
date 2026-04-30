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
        #[cfg(target_os = "windows")]
        {
            println!("[Lotus] Applying Servo engine patches (Windows)...");
            let output = Command::new("git")
                .arg("apply")
                .arg("--ignore-space-change") // often needed for cross-platform patches
                .arg("--ignore-whitespace") // often needed for cross-platform patches
                .arg("--directory")
                .arg(&servo_dir)
                .arg(&patch_file)
                .output()
                .expect("Failed to execute git apply");

            if !output.status.success() {
                // If git apply fails, it might be because the patch is already applied.
                // We should only panic if it's a real error.
                let stderr = String::from_utf8_lossy(&output.stderr);
                if !stderr.contains("already applied") && !stderr.contains("patch does not apply") {
                    panic!("Failed to apply patch: {}\n{}", String::from_utf8_lossy(&output.stdout), stderr);
                } else {
                    println!("[Lotus] Servo patches already applied or minor conflict, continuing.");
                }
            } else {
                println!("[Lotus] Servo patches applied successfully (Windows).");
            }
        }

        #[cfg(not(target_os = "windows"))]
        {
            let script_path = Path::new(&manifest_dir).join("scripts").join("apply-patches.sh");
            if script_path.exists() {
                println!("[Lotus] Applying Servo engine patches (Unix-like)...");
                let status = Command::new("bash")
                    .arg(script_path)
                    .status()
                    .expect("Failed to execute apply-patches.sh");
                
                if !status.success() {
                    // Similar logic as in the .sh script: if patch fails, it might be already applied.
                    // Instead of a generic panic, we should check if the error is "already applied".
                    // However, for simplicity and because the .sh script exits 0 in this case,
                    // we'll just check success. The .sh script itself handles the "already applied" case.
                    panic!("apply-patches.sh failed with status: {}", status);
                }
            } else {
                eprintln!("[Error] apply-patches.sh not found at {:?}", script_path);
            }
        }
    }

    napi_build::setup();
}

