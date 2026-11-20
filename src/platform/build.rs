fn main() {
    let dir = std::path::PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    println!("cargo:rustc-link-search={}", dir.display());
    println!("cargo:rustc-link-arg=-Tlink.x");
    println!("cargo:rerun-if-changed=link.x");
    println!("cargo:rerun-if-changed=entry.S");
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    for path in [
        "../../tools/display_assets.py",
        "../../tools/flash_assets.py",
        "../../vela_boot_and_display_assets/vela_boot.png",
        "../../vela_boot_and_display_assets/vela_display_test.png",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let status = std::process::Command::new("python3")
        .arg("../../tools/display_assets.py")
        .arg(&out)
        .arg("--metadata-only")
        .status()
        .expect("Python3 and Pillow required for display assets");
    assert!(status.success(), "convert display PNG assets");
    build_sentinel(&dir, &out);
    build_user_services(&dir, &out);
    let linker = user_linker();
    for (name, pie) in [("hello", false), ("pic", true), ("dma", true)] {
        let source = format!("../user/apps/{name}/main.S");
        let layout = if pie { "pic" } else { name };
        let script = format!("../user/apps/{layout}/link.x");
        println!("cargo:rerun-if-changed={source}");
        println!("cargo:rerun-if-changed={script}");
        let status = std::process::Command::new("clang")
            .args([
                "--target=riscv32-none-elf",
                "-march=rv32imac",
                "-mabi=ilp32",
                "-nostdlib",
                "-Wl,--strip-all",
                "-Xlinker",
                "-T",
                "-Xlinker",
                &script,
                &source,
            ])
            .arg(&linker)
            .args(pie.then_some("-Wl,-pie"))
            .arg("-o")
            .arg(out.join(format!("{name}.elf")))
            .status()
            .expect("clang and ld.lld required for bundled user ELF");
        assert!(status.success(), "building {name}.elf failed");
    }
}

fn build_sentinel(dir: &std::path::Path, out: &std::path::Path) {
    for path in [
        "../lp/Cargo.toml",
        "../lp/link.x",
        "../lp/src/lib.rs",
        "../lp/src/journal.rs",
        "../lp/src/wakeup.rs",
        "../lp/src/watch.rs",
        "../lp/src/hw/mod.rs",
        "../lp/src/hw/lp_counter.rs",
        "../lp/src/hw/mailbox.rs",
        "../lp/src/bin/sentinel.rs",
        "../abi/Cargo.toml",
        "../abi/src/lib.rs",
        "../abi/src/sentinel.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let manifest = dir.join("../lp/Cargo.toml");
    let script = dir.join("../lp/link.x");
    let lp_target = out.join("lp-target");
    let flags = format!(
        "-Crelocation-model=static\x1f-Clink-arg=--oformat=binary\x1f-Clink-arg=-T{}",
        script.display()
    );
    // Link directly to a flat load image. LP initializes its own NOLOAD BSS.
    let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", flags)
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args([
            "--bin",
            "vela-sentinel",
            "--release",
            "--locked",
            "--target",
            "riscv32imac-unknown-none-elf",
            "--target-dir",
        ])
        .arg(&lp_target)
        .status()
        .expect("run cargo for Sentinel");
    assert!(status.success(), "building LP Sentinel failed");
    std::fs::copy(
        lp_target.join("riscv32imac-unknown-none-elf/release/vela-sentinel"),
        out.join("sentinel.bin"),
    )
    .expect("copy sentinel.bin");
}

fn build_user_services(dir: &std::path::Path, out: &std::path::Path) {
    for path in [
        "../user/Cargo.toml",
        "../user/link.x",
        "../user/src/lib.rs",
        "../user/src/sys.rs",
        "../user/src/arena.rs",
        "../user/src/ina226.rs",
        "../user/src/sdcard.rs",
        "../user/src/bin/procfs.rs",
        "../user/src/bin/shell.rs",
        "../user/src/bin/console.rs",
        "../abi/Cargo.toml",
        "../abi/src/lib.rs",
        "../abi/src/syscall.rs",
    ] {
        println!("cargo:rerun-if-changed={path}");
    }
    let manifest = dir.join("../user/Cargo.toml");
    let script = dir.join("../user/link.x");
    let user_target = out.join("user-target");
    let flags = format!(
        "-Crelocation-model=pie\x1f-Cstrip=symbols\x1f-Clink-arg=-pie\x1f-Clink-arg=-zmax-page-size=64\x1f-Clink-arg=-T{}",
        script.display()
    );
    let status = std::process::Command::new(std::env::var_os("CARGO").unwrap())
        .env_remove("RUSTFLAGS")
        .env("CARGO_ENCODED_RUSTFLAGS", flags)
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args([
            "--bins",
            "--release",
            "--locked",
            "--target",
            "riscv32imac-unknown-none-elf",
            "--target-dir",
        ])
        .arg(&user_target)
        .status()
        .expect("run cargo for vela-user services");
    assert!(status.success(), "building vela-user ELF services failed");
    for (bin, artifact) in [
        ("vela-procfs", "procfs.elf"),
        ("vela-shell", "shell.elf"),
        ("vela-console", "console.elf"),
    ] {
        std::fs::copy(
            user_target
                .join("riscv32imac-unknown-none-elf/release")
                .join(bin),
            out.join(artifact),
        )
        .unwrap_or_else(|e| panic!("copy {artifact}: {e}"));
    }
}

// Clang is required for the bundled user assembly; a separately installed LLD
// is optional because the pinned Rust toolchain already supplies GNU-flavour LLD.
fn user_linker() -> String {
    if std::process::Command::new("ld.lld")
        .arg("--version")
        .output()
        .is_ok_and(|output| output.status.success())
    {
        return "-fuse-ld=lld".into();
    }
    let output = std::process::Command::new(std::env::var_os("RUSTC").unwrap())
        .args(["--print", "sysroot"])
        .output()
        .expect("query Rust sysroot");
    assert!(output.status.success(), "query Rust sysroot failed");
    let root = String::from_utf8(output.stdout).expect("UTF-8 Rust sysroot");
    let linker = std::path::Path::new(root.trim())
        .join("lib/rustlib")
        .join(std::env::var_os("HOST").unwrap())
        .join("bin/gcc-ld/ld.lld");
    assert!(
        linker.is_file(),
        "install ld.lld: Rust bundled linker not found"
    );
    format!("--ld-path={}", linker.display())
}
