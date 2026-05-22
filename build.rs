use std::env;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn zig_target(target: &str) -> String {
    let mut parts = target.split('-');
    let arch = clang_arch(parts.next().unwrap_or(target));
    let _vendor = parts.next();
    let rest = parts.collect::<Vec<_>>();

    if rest.is_empty() {
        target.to_owned()
    } else {
        format!("{arch}-{}", rest.join("-"))
    }
}

fn clang_arch(arch: &str) -> &str {
    match arch {
        "riscv64gc" => "riscv64",
        _ => arch,
    }
}

fn clang_target(target: &str) -> String {
    let mut parts = target.split('-');
    let arch = clang_arch(parts.next().unwrap_or(target));
    let rest = parts.collect::<Vec<_>>();

    if rest.is_empty() {
        arch.to_owned()
    } else {
        format!("{arch}-{}", rest.join("-"))
    }
}

fn clang_target_args(target: &str) -> Vec<String> {
    let mut args = vec![format!("--target={}", clang_target(target))];
    if target.starts_with("riscv64gc-") {
        args.push("-march=rv64gc".to_owned());
    }
    args
}

fn zig_include_dirs(target: &str) -> Option<Vec<String>> {
    let args = [
        "cc".to_owned(),
        format!("--target={}", zig_target(target)),
        "-E".to_owned(),
        "-x".to_owned(),
        "c".to_owned(),
        "-".to_owned(),
        "-v".to_owned(),
    ];

    let output = Command::new("zig")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stderr = String::from_utf8(output.stderr).ok()?;
    let start = stderr.find("#include <...> search starts here:")?;
    let end = stderr[start..].find("End of search list.")? + start;
    let include_block = &stderr[start..end];

    let include_dirs = include_block
        .lines()
        .skip(1)
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with("#include"))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();

    Some(include_dirs)
}

fn is_zigbuild(target: &str) -> bool {
    let cc_key = format!("CC_{}", target.replace('-', "_"));
    if let Ok(cc) = env::var(&cc_key) {
        return cc.contains("zigcc");
    }
    if let Ok(cc) = env::var("CC") {
        return cc.contains("zigcc");
    }
    false
}

fn generate_bindings(target: &str) {
    let extra_header_path = env::var("KCP_SYS_EXTRA_HEADER_PATH").unwrap_or_default();
    let extra_header_paths = extra_header_path
        .split(':')
        .filter(|s| !s.is_empty())
        .map(|p| format!("-I{p}"));

    let mut bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_function("ikcp_.*")
        .use_core();

    bindings = bindings.clang_args(extra_header_paths);

    let host = env::var("HOST").unwrap();
    if target != host {
        if is_zigbuild(target) {
            bindings = bindings.clang_args(clang_target_args(target));

            if let Some(include_dirs) = zig_include_dirs(target) {
                for include_dir in include_dirs {
                    bindings = bindings.clang_arg("-isystem").clang_arg(include_dir);
                }
            }
        } else {
            bindings = bindings.clang_args(clang_target_args(target));
        }
    }

    let bindings = bindings.generate().expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap()).join("bindings.rs");
    bindings
        .write_to_file(out_path)
        .expect("Couldn't write bindings!");
}

fn find_llvm_ar() -> Option<PathBuf> {
    // ARM Mac (Apple Silicon)
    let arm_path = PathBuf::from("/opt/homebrew/opt/llvm/bin/llvm-ar");
    if arm_path.exists() {
        return Some(arm_path);
    }
    // Intel Mac
    let intel_path = PathBuf::from("/usr/local/opt/llvm/bin/llvm-ar");
    if intel_path.exists() {
        return Some(intel_path);
    }
    // Fallback: check PATH (MacPorts, Nix, manual installs, etc.)
    Command::new("which")
        .arg("llvm-ar")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| {
            let path = String::from_utf8(o.stdout).ok()?;
            let path = path.trim();
            if path.is_empty() {
                None
            } else {
                Some(PathBuf::from(path))
            }
        })
}

fn main() {
    println!("cargo:rustc-link-lib=kcp");
    let dir = env::var("CARGO_MANIFEST_DIR").unwrap();
    let fulldir = Path::new(&dir).join("kcp");
    let target = env::var("TARGET").unwrap();

    let mut config = cc::Build::new();
    if target.contains("apple-darwin") {
        if let Some(llvm_ar) = find_llvm_ar() {
            config.archiver(llvm_ar);
        }
    }
    config.include(fulldir.clone());
    config.file(fulldir.join("ikcp.c"));
    config.opt_level(3);
    config.warnings(false);
    config.compile("libkcp.a");
    println!("cargo:rustc-link-search=native={}", fulldir.display());

    println!("cargo:rerun-if-changed=kcp/ikcp.h");
    println!("cargo:rerun-if-changed=kcp/ikcp.c");
    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-env-changed=KCP_SYS_EXTRA_HEADER_PATH");
    println!("cargo:rerun-if-env-changed=CC_{}", target.replace('-', "_"));
    println!("cargo:rerun-if-env-changed=CC");

    generate_bindings(&target);
}
