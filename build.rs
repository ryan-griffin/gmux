use std::{
    env,
    path::{Path, PathBuf},
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
    let bindings = bindgen::Builder::default()
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
        .allowlist_var("GMUX_(UNDERLINE|MOUSE_SHAPE|DECORATION|PROGRESS|LINK)_.*")
        .allowlist_function("gmux_engine_.*")
        .allowlist_function("gmux_checkpoint_history")
        .use_core()
        .derive_debug(false)
        .derive_partialeq(true)
        .derive_eq(true)
        .rust_edition(bindgen::RustEdition::Edition2024)
        // Bindgen emits const assertions for size, alignment,
        // and every member offset. They run in all builds, including cross builds.
        .layout_tests(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .parse_callbacks(Box::new(BridgeCallbacks))
        .generate()
        .expect("Could not generate the Gmux C/Rust ABI declarations");
    let generated = bindings.to_string();
    // Keep bindgen's C-derived size, alignment, and field-offset assertions, but
    // use the client library's values at the FFI boundary. The client needs no C build.
    let mut declarations = generated.clone();
    for name in ["Cell", "Frame", "Scrollback"] {
        let marker = format!("pub struct {name} {{");
        let start = declarations
            .find(&marker)
            .expect("Missing viewport ABI type");
        let end = start + declarations[start..].find("\n}").unwrap() + 2;
        let attributes = declarations[..start].rfind("#[repr(C)]").unwrap();
        declarations.replace_range(attributes..end, "");
    }
    // Check every named protocol code against the toolkit-free client values.
    // The C implementation maps pinned Ghostty enum names explicitly.
    for (prefix, module) in [
        ("GMUX_UNDERLINE_", "cells::underline"),
        ("GMUX_MOUSE_SHAPE_", "cells::mouse_shape"),
        ("GMUX_DECORATION_", "cells::decoration"),
        ("GMUX_LINK_", "hyperlinks"),
    ] {
        let mut checked = 0;
        for line in generated.lines() {
            if let Some(rest) = line.strip_prefix(&format!("pub const {prefix}")) {
                let (name, _) = rest.split_once(':').unwrap();
                declarations.push_str(&format!(
                    "const _: () = assert!({prefix}{name} == gmux_client::{module}::{name} as u32);\n"
                ));
                checked += 1;
            }
        }
        assert!(checked > 0, "Missing bridge constants for {module}");
    }
    std::fs::write(
        out_dir.join("ghostty_bridge.rs"),
        format!("pub use gmux_client::{{Cell,Frame,Scrollback}};\n{declarations}"),
    )
    .expect("Could not write ABI bindings");
}

fn main() {
    println!("cargo:rerun-if-changed=src/ghostty_bridge.c");
    println!("cargo:rerun-if-changed=src/ghostty_bridge.h");
    println!("cargo:rerun-if-changed=src/ghostty_links.h");
    println!("cargo:rerun-if-env-changed=GHOSTTY_PREFIX");
    let root = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rerun-if-changed=scripts/bootstrap-ghostty.py");
    let bootstrap = std::fs::read_to_string(root.join("scripts/bootstrap-ghostty.py")).unwrap();
    let revision = bootstrap
        .lines()
        .find_map(|line| {
            line.strip_prefix("REVISION = \"")
                .and_then(|s| s.strip_suffix('"'))
        })
        .expect("Ghostty source revision is missing");
    assert!(revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()));
    println!("cargo:rustc-env=GMUX_GHOSTTY_REVISION={revision}");
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
        .define(
            "GMUX_VERSION",
            concat!("\"", env!("CARGO_PKG_VERSION"), "\""),
        )
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
    println!("cargo::metadata=lib_dir={}", prefix.join("lib").display());
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/gmux");
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        prefix.join("lib").display()
    );
}
