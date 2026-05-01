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
            let script_path = Path::new(&manifest_dir).join("scripts").join("apply-patches.ps1");
            println!("cargo:rerun-if-changed={}", script_path.display());

            if script_path.exists() {
                println!("[Lotus] Applying Servo engine patches (Windows PowerShell)...");
                let status = Command::new("powershell")
                    .arg("-File")
                    .arg(&script_path)
                    .status()
                    .expect("Failed to execute apply-patches.ps1");

                if !status.success() {
                    panic!("apply-patches.ps1 failed with status: {}", status);
                }
            } else {
                eprintln!("[Error] apply-patches.ps1 not found at {:?}", script_path);
                panic!("Patch script not found, cannot proceed with Windows build.");
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

