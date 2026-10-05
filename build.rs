fn main() {
    println!(
        "cargo:rustc-env=HAPS_TARGET={}",
        std::env::var("TARGET").unwrap()
    );
}
