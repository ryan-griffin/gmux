use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("../..");
    let data = root.join("data");
    println!("cargo:rerun-if-changed={}", data.display());
    let resources = PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("gmux.gresource");
    let status = Command::new("glib-compile-resources")
        .arg("--sourcedir")
        .arg(&data)
        .arg("--target")
        .arg(resources)
        .arg(data.join("gmux.gresource.xml"))
        .status()
        .expect("glib-compile-resources is required to bundle the app's icons");
    assert!(
        status.success(),
        "Could not compile the app's icon resources"
    );
}
