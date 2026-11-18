fn main() {
    let dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-link-search={dir}");
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=entry.S");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    build_sentinel(&out);
    for (name, pie) in [("hello", false), ("pic", true), ("dma", true)] {
        let source = format!("../../userspace/{name}/main.S");
        let layout = if pie { "pic" } else { name };
        let script = format!("../../userspace/{layout}/link.x");
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

fn build_sentinel(out: &std::path::Path) {
    let source = "../../lp/sentinel/main.rs";
    let script = "../../lp/sentinel/link.x";
    for path in [
        source,
        script,
        "../../lp/sentinel/watch.rs",
        "../../soc/esp32p4-v13/lp_counter.rs",
        "../../abi/sentinel.rs",
        "../../soc/esp32p4-v13/mailbox.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    // Link directly to a flat load image. LP initializes its own NOLOAD BSS.
    let status = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .args([
            "--edition=2024",
            "--crate-name=vela_sentinel",
            "--target=riscv32imac-unknown-none-elf",
            "-Cpanic=abort",
            "-Copt-level=s",
            "-Crelocation-model=static",
            "-Clink-arg=--oformat=binary",
        ])
        .arg(format!("-Clink-arg=-T{script}"))
        .arg(source)
        .arg("-o")
        .arg(out.join("sentinel.bin"))
        .status()
        .expect("run rustc for Sentinel");
    assert!(status.success(), "building LP Sentinel failed");
}
