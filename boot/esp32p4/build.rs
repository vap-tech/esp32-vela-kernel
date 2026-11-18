fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={dir}");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=entry.S");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let source = "../../userspace/hello/main.S";
    let script = "../../userspace/hello/link.x";
    println!("cargo:rerun-if-changed={source}");
    println!("cargo:rerun-if-changed={script}");
    let status = std::process::Command::new("clang")
        .args([
            "--target=riscv32-none-elf",
            "-march=rv32imac",
            "-mabi=ilp32",
            "-fuse-ld=lld",
            "-nostdlib",
            "-Wl,--strip-all",
            "-Xlinker",
            "-T",
            "-Xlinker",
            script,
            source,
            "-o",
        ])
        .arg(out.join("hello.elf"))
        .status()
        .expect("clang and ld.lld required for hello.elf");
    assert!(status.success(), "building hello.elf failed");
}
