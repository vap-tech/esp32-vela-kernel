fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={dir}");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=entry.S");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    for (name, pie) in [("hello", false), ("pic", true)] {
        let source = format!("../../userspace/{name}/main.S");
        let script = format!("../../userspace/{name}/link.x");
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
                &script,
                &source,
            ])
            .args(pie.then_some("-Wl,-pie"))
            .arg("-o")
            .arg(out.join(format!("{name}.elf")))
            .status()
            .expect("clang and ld.lld required for hello.elf");
        assert!(status.success(), "building hello.elf failed");
    }
}
