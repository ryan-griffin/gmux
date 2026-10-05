use std::{env, path::PathBuf, process::Command};

fn main() {
    println!("cargo:rerun-if-changed=src/ghostty_bridge.c");
    println!("cargo:rerun-if-env-changed=GHOSTTY_PREFIX");
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=data/gmux.gresource.xml");
    println!("cargo:rerun-if-changed=data/dev.gmux.Gmux.svg");
    println!("cargo:rerun-if-changed=data/dev.gmux.Gmux-symbolic.svg");
    println!("cargo:rerun-if-changed=data/icons/branch-symbolic.svg");
    let resources = PathBuf::from(env::var("OUT_DIR").unwrap()).join("gmux.gresource");
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(root.join("data"))
        .arg("--target")
        .arg(resources)
        .arg(root.join("data/gmux.gresource.xml"))
        .status()
        .expect("glib-compile-resources is required to bundle the app's icons");
    assert!(
        status.success(),
        "Could not compile the app's icon resources"
    );
    let prefix = env::var_os("GHOSTTY_PREFIX")
        .map(PathBuf::from)
        .unwrap_or_else(|| root.join(".deps/ghostty"));
    // Native headers and libraries can change without a Rust source edit.
    println!(
        "cargo:rerun-if-changed={}",
        prefix.join("include").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        prefix.join("lib/libghostty-vt.so").display()
    );
    if !prefix.join("include/ghostty/vt/terminal.h").exists() {
        panic!(
            "libghostty-vt terminal API is missing. Run ./scripts/bootstrap-ghostty.sh first, or set GHOSTTY_PREFIX to a compatible installation."
        );
    }
    cc::Build::new()
        .file("src/ghostty_bridge.c")
        .include(prefix.join("include"))
        .flag("-std=c11")
        .warnings(true)
        .compile("gmux-ghostty");
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=dylib=ghostty-vt");
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/gmux");
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        prefix.join("lib").display()
    );
}
