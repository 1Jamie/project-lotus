fn main() {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let patch = manifest.join("../lotus-core/patches/servo-v0.7.0-webgl-broker.patch");
    let servo_webgl = manifest.join("../lotus-core/servo/components/webgl/webgl_thread.rs");
    let servo_paint_lib = manifest.join("../lotus-core/servo/components/shared/paint/lib.rs");
    println!("cargo:rerun-if-changed={}", patch.display());
    println!("cargo:rerun-if-changed={}", servo_webgl.display());
    println!("cargo:rerun-if-changed={}", servo_paint_lib.display());
}
