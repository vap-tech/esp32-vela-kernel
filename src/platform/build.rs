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
    println!("cargo:rerun-if-env-changed=VELA_BOARD_INIT_FILE");
    let board_init_path = std::env::var_os("VELA_BOARD_INIT_FILE")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| dir.join("../../boards/tab5/init.conf"));
    println!("cargo:rerun-if-changed={}", board_init_path.display());
    let board_init_text = std::fs::read_to_string(&board_init_path)
        .unwrap_or_else(|e| panic!("read board init config {}: {e}", board_init_path.display()));

    for path in [
        "../user/Cargo.toml",
        "../user/link.x",
        "../user/src/lib.rs",
        "../user/src/sys.rs",
        "../user/src/arena.rs",
        "../user/src/boot_cfg.rs",
        "../user/src/ina226.rs",
        "../user/src/i2c.rs",
        "../user/src/kbd.rs",
        "../user/src/fbcon.rs",
        "../user/src/file.rs",
        "../user/src/mount.rs",
        "../user/src/ext4.rs",
        "../user/src/bin/ext4fs.rs",
        "../user/src/font.rs",
        "../user/src/term.rs",
        "../user/src/sdcard.rs",
        "../user/src/bin/sysd/main.rs",
        "../user/src/bin/sysd/proc.rs",
        "../user/src/bin/sysd/sys.rs",
        "../user/src/bin/sysd/dev.rs",
        "../user/src/bin/init.rs",
        "../user/src/bin/console.rs",
        "../user/src/bin/kbd.rs",
        "../user/src/bin/fbcon.rs",
        "../abi/Cargo.toml",
        "../abi/src/lib.rs",
        "../abi/src/fs.rs",
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
        .env("VELA_BOARD_INIT", &board_init_text)
        .args(["build", "--manifest-path"])
        .arg(&manifest)
        .args([
            "--bin",
            "vela-sysd",
            "--bin",
            "vela-ext4fs",
            "--bin",
            "vela-init",
            "--bin",
            "vela-console",
            "--bin",
            "vela-kbd",
            "--bin",
            "vela-fbcon",
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
        ("vela-sysd", "sysd.elf"),
        ("vela-ext4fs", "ext4fs.elf"),
        ("vela-init", "init.elf"),
        ("vela-console", "console.elf"),
        ("vela-kbd", "kbd.elf"),
        ("vela-fbcon", "fbcon.elf"),
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
