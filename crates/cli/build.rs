use std::env;

fn main() {
    println!("cargo:rustc-link-arg=-Wl,-rpath,$ORIGIN/../lib/gmux");
    println!(
        "cargo:rustc-link-arg=-Wl,-rpath,{}",
        env::var("DEP_GMUX_GHOSTTY_LIB_DIR").unwrap()
    );
}
