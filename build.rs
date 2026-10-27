use std::{env, path::PathBuf, process::Command};

fn main() {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let prefix = root.join(".tmp/native/prefix");
    if !prefix.join("lib/libghostty-vt.a").exists() {
        let status = Command::new("bash")
            .arg(root.join("scripts/native.sh"))
            .arg(root.join(".tmp/native"))
            .args(["native", "prefix", "zig-cache"])
            .status()
            .expect("Zig 0.16.0, bash, curl, tar, shasum and pkg-config are required");
        assert!(
            status.success(),
            "building checksum-pinned libghostty-vt failed"
        );
    }
    let include = prefix.join("include");
    let zig_env = Command::new("zig")
        .arg("env")
        .output()
        .expect("Zig 0.16.0 is required");
    assert!(zig_env.status.success(), "reading Zig installation failed");
    let zig_env = String::from_utf8(zig_env.stdout).unwrap();
    let zig_lib = zig_env
        .lines()
        .find_map(|line| {
            line.trim()
                .strip_prefix(".lib_dir = \"")
                .and_then(|s| s.strip_suffix("\","))
        })
        .expect("Zig 0.16.0 must report its library directory");
    let bindings = bindgen::Builder::default()
        .header(include.join("ghostty/vt.h").to_string_lossy())
        .clang_arg(format!("-I{}", include.display()))
        .clang_arg("-isystem")
        .clang_arg(PathBuf::from(zig_lib).join("include").to_string_lossy())
        .allowlist_type("Ghostty.*")
        .allowlist_function("ghostty_.*")
        .allowlist_var("GHOSTTY_.*")
        .derive_default(true)
        .layout_tests(false)
        .prepend_enum_name(false)
        .generate_comments(false)
        .generate()
        .expect("generating native Ghostty bindings requires libclang");
    bindings
        .write_to_file(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("ghostty.rs"))
        .unwrap();
    if env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        bindgen::Builder::default()
            .header_contents("process_osx.h", "#include <sys/sysctl.h>\n#include <sys/proc.h>\n#include <sys/proc_info.h>\n#include <mach/mach_time.h>\n")
            .allowlist_type("kinfo_proc|extern_proc|eproc|mach_timebase_info_data_t")
            .allowlist_function("mach_timebase_info")
            .allowlist_var("PROC_FLAG_CTTY|PROC_FLAG_LP64|P_WEXIT|SZOMB")
            .derive_default(true).layout_tests(false).generate_comments(false)
            .generate().expect("generating process bindings requires the macOS SDK")
            .write_to_file(PathBuf::from(env::var_os("OUT_DIR").unwrap()).join("process_osx.rs")).unwrap();
    }
    println!(
        "cargo:rustc-link-search=native={}",
        prefix.join("lib").display()
    );
    println!("cargo:rustc-link-lib=static=ghostty-vt");
    println!("cargo:rerun-if-changed=scripts/native.sh");
    println!("cargo:rerun-if-changed=build.rs");
}
