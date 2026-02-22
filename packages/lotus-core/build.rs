use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    napi_build::setup();

    if env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    // target/{debug,release}/build/lotus-core-<hash>/out -> target/{debug,release}/build
    let build_dir = out_dir.ancestors().nth(2).expect("Failed to resolve build directory");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let dest_dir = manifest_dir.join("windows");
    
    fs::create_dir_all(&dest_dir).expect("Failed to create windows output directory");

    let mut found_egl = false;
    
    if let Ok(entries) = fs::read_dir(&build_dir) {
        // Sort entries by modified time (newest first) to ensure we get the latest build
        let mut dirs: Vec<_> = entries.flatten().filter(|e| e.path().is_dir()).collect();
        dirs.sort_by_key(|a| std::cmp::Reverse(a.metadata().and_then(|m| m.modified()).unwrap_or(std::time::SystemTime::UNIX_EPOCH)));

        for entry in dirs {
            let path = entry.path();
            let dir_name = path.file_name().unwrap_or_default().to_string_lossy();
            
            if dir_name.starts_with("mozangle-") {
                let mozangle_out = path.join("out");
                let egl_src = mozangle_out.join("libEGL.dll");
                let gles_src = mozangle_out.join("libGLESv2.dll");

                if egl_src.exists() && gles_src.exists() {
                    fs::copy(&egl_src, dest_dir.join("libEGL.dll")).expect("Failed to copy libEGL.dll");
                    fs::copy(&gles_src, dest_dir.join("libGLESv2.dll")).expect("Failed to copy libGLESv2.dll");
                    
                    let d3d_src = mozangle_out.join("d3dcompiler_47.dll");
                    if d3d_src.exists() {
                        fs::copy(&d3d_src, dest_dir.join("d3dcompiler_47.dll")).ok();
                    }
                    
                    println!("cargo:warning=Extracted ANGLE DLLs to {}", dest_dir.display());
                    found_egl = true;
                    break;
                }
            }
        }
    }

    if !found_egl {
        panic!("libEGL.dll or libGLESv2.dll not found in {:?}. Ensure the 'no-wgl' feature is enabled on libservo.", build_dir);
    }
}
