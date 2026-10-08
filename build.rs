use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug)]
struct BridgeCallbacks;

impl bindgen::callbacks::ParseCallbacks for BridgeCallbacks {
    fn item_name(&self, info: bindgen::callbacks::ItemInfo<'_>) -> Option<String> {
        match info.name {
            "GmuxCell" => Some("Cell".into()),
            "GmuxFrame" => Some("Frame".into()),
            "GmuxScrollback" => Some("Scrollback".into()),
            _ => None,
        }
    }

    fn add_derives(&self, info: &bindgen::callbacks::DeriveInfo<'_>) -> Vec<String> {
        match info.name {
            "Frame" => vec!["Default", "serde::Serialize", "serde::Deserialize"],
            "Scrollback" => vec!["Default", "Debug", "serde::Serialize", "serde::Deserialize"],
            _ => vec![],
        }
        .into_iter()
        .map(String::from)
        .collect()
    }
}

fn generate_bridge_bindings(build: &cc::Build, out_dir: &Path) {
    // Parse only our stable shim, never Ghostty's implementation types. Clang
    // uses Cargo's target, so the declarations and layout assertions also work
    // when building on one supported architecture for the other. Use the C
    // compiler's flags too, including ABI options and target header paths.
    bindgen::Builder::default()
        .header("src/ghostty_bridge.h")
        .clang_args(
            build
                .get_compiler()
                .args()
                .iter()
                .map(|arg| arg.to_string_lossy()),
        )
        .clang_arg(format!("--target={}", env::var("TARGET").unwrap()))
        .allowlist_type("Gmux.*")
        .allowlist_function("gmux_engine_.*")
        .use_core()
        .derive_debug(false)
        .derive_partialeq(true)
        .derive_eq(true)
        .rust_target(bindgen::RustTarget::stable(85, 0).unwrap())
        .rust_edition(bindgen::RustEdition::Edition2024)
        // With Rust 1.77+, bindgen emits const assertions for size, alignment,
        // and every member offset. They run in all builds, including cross builds.
        .layout_tests(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(BridgeCallbacks))
        .generate()
        .expect("Could not generate the Gmux C/Rust ABI declarations")
        .write_to_file(out_dir.join("ghostty_bridge.rs"))
        .expect("Could not write the Gmux C/Rust ABI declarations");
}

fn main() {
    println!("cargo:rerun-if-changed=src/ghostty_bridge.c");
    println!("cargo:rerun-if-changed=src/ghostty_bridge.h");
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
    let mut bridge = cc::Build::new();
    bridge
        .file("src/ghostty_bridge.c")
        .include(prefix.join("include"))
        .flag("-std=c11")
        // Every exported definition must have a prototype in the shared header.
        .flag("-Werror=missing-prototypes")
        .warnings(true);
    generate_bridge_bindings(&bridge, &PathBuf::from(env::var_os("OUT_DIR").unwrap()));
    bridge.compile("gmux-ghostty");
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
