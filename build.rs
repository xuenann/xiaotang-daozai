//! 把岛仔图标作为 RT_GROUP_ICON #1 编进 exe，供 LoadImageW(hInst, 1, ...) 取用。
//! 优先用 Windows SDK 的 rc.exe；没有 SDK 时回退到 mingw 的 windres，两者产出都能交给链接器。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn find_rc() -> Option<PathBuf> {
    let roots = [
        r"C:\Program Files (x86)\Windows Kits\10\bin",
        r"C:\Program Files\Windows Kits\10\bin",
    ];
    let mut found: Vec<PathBuf> = Vec::new();
    for root in roots {
        let Ok(entries) = fs::read_dir(root) else { continue };
        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            let rc = dir.join("x64").join("rc.exe");
            if rc.exists() {
                found.push(rc);
            }
        }
    }
    found.sort();
    found.pop()
}

/// 没有 Windows SDK 时的后备：mingw 的 windres 同样能编译资源脚本。
fn find_windres() -> Option<PathBuf> {
    for name in ["windres.exe", "windres"] {
        let works = Command::new(name)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false);
        if works {
            return Some(PathBuf::from(name));
        }
    }
    [
        r"C:\mingw64\bin\windres.exe",
        r"C:\msys64\mingw64\bin\windres.exe",
        r"C:\Program Files\mingw64\bin\windres.exe",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.exists())
}

fn main() {
    println!("cargo:rerun-if-changed=app.rc");
    println!("cargo:rerun-if-changed=assets/daozai.ico");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out = PathBuf::from(env::var("OUT_DIR").unwrap()).join("daozai.res");
    if let Some(rc) = find_rc() {
        let status = Command::new(&rc)
            .arg("/nologo")
            .arg("/fo")
            .arg(&out)
            .arg("app.rc")
            .status();
        if status.map(|s| s.success()).unwrap_or(false) {
            println!("cargo:rustc-link-arg={}", out.display());
            return;
        }
        println!("cargo:warning=rc.exe 编译 app.rc 失败，改用 windres");
    }
    let object = Path::new(&env::var("OUT_DIR").unwrap()).join("daozai-res.o");
    let Some(windres) = find_windres() else {
        println!("cargo:warning=rc.exe 与 windres 都没找到，跳过图标资源");
        return;
    };
    let status = Command::new(&windres)
        .arg("--codepage=65001")
        .arg("-I").arg(".")
        .arg("-i").arg("app.rc")
        .arg("-O").arg("coff")
        .arg("-o").arg(&object)
        .status();
    if status.map(|s| s.success()).unwrap_or(false) {
        println!("cargo:rustc-link-arg={}", object.display());
    } else {
        println!("cargo:warning=windres 编译 app.rc 失败，跳过图标资源");
    }
}
