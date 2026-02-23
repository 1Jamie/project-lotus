use std::env;
use std::fs;
use std::path::{Path, PathBuf};

fn main() {
    napi_build::setup();

    if env::var("CARGO_CFG_WINDOWS").is_err() {
        return;
    }

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    
    // Check multiple potential build directories since Cargo workspace target dirs can vary
    let mut possible_build_dirs = Vec::new();

    // 1. The local target directory derived from OUT_DIR
    // target/{debug,release}/build/lotus-core-<hash>/out -> target/{debug,release}/build
    // target/<target-triple>/{debug,release}/build/lotus-core-<hash>/out -> target/<target-triple>/{debug,release}/build
    if let Some(build_dir) = out_dir.ancestors().nth(2) {
        possible_build_dirs.push(build_dir.to_path_buf());
    }

    // 2. The workspace target directory (e.g., if we're in packages/lotus-core, check ../../target/...)
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let workspace_dir = manifest_dir.parent().and_then(|p| p.parent());
    
    if let Some(ws_dir) = workspace_dir {
        // Try to construct equivalent path in workspace root
        if let Some(build_dir) = out_dir.ancestors().nth(2) {
            // Find relative path from manifest_dir to build_dir
            if let Ok(rel_path) = build_dir.strip_prefix(&manifest_dir) {
                possible_build_dirs.push(ws_dir.join(rel_path));
            } else {
                // Heuristic fallback for workspace root
                let profile = env::var("PROFILE").unwrap_or_else(|_| "release".to_string());
                let target = env::var("TARGET").unwrap_or_default();
                
                let ws_build_dir = if target.is_empty() {
                    ws_dir.join("target").join(&profile).join("build")
                } else {
                    ws_dir.join("target").join(&target).join(&profile).join("build")
                };
                possible_build_dirs.push(ws_build_dir);
            }
        }
    }

    let dest_dir = manifest_dir.join("windows");
    fs::create_dir_all(&dest_dir).expect("Failed to create windows output directory");

    let mut found_egl = false;

    for build_dir in &possible_build_dirs {
        if !build_dir.exists() {
            continue;
        }

        if let Ok(entries) = fs::read_dir(build_dir) {
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
                        if let (Ok(_), Ok(_)) = (
                            fs::copy(&egl_src, dest_dir.join("libEGL.dll")),
                            fs::copy(&gles_src, dest_dir.join("libGLESv2.dll"))
                        ) {
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
        }

        if found_egl {
            break;
        }
    }

    if !found_egl {
        panic!(
            "libEGL.dll or libGLESv2.dll not found. Searched directories: {:?}. Ensure the 'no-wgl' feature is enabled on libservo.",
            possible_build_dirs
        );
    }
}
