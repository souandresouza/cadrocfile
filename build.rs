//! Compiles `.po` translation files into `.mo` at build time.
//!
//! Looks for `po/<lang>.po` and produces `po/<lang>.mo` next to it.
//! The `.mo` files are installed alongside the binary by `install.sh`.

use std::env;
use std::fs;
use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=po");

    let out_dir = env::var("OUT_DIR").unwrap();
    let po_dir = Path::new("po");

    if !po_dir.is_dir() {
        return;
    }

    for entry in fs::read_dir(po_dir).expect("read po dir") {
        let entry = entry.expect("po entry");
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("po") {
            continue;
        }

        let lang = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
        let mo_path = po_dir.join(format!("{lang}.mo"));

        // Compile .po -> .mo using msgfmt (from gettext)
        let status = Command::new("msgfmt")
            .arg("--output-file")
            .arg(&mo_path)
            .arg(&path)
            .status();

        match status {
            Ok(s) if s.success() => {
                println!("cargo:warning=Compiled {} -> {}", path.display(), mo_path.display());
            }
            Ok(_) => {
                println!("cargo:warning=msgfmt failed for {}", path.display());
            }
            Err(_) => {
                println!("cargo:warning=msgfmt not found; translations for {} will not be available", lang);
            }
        }
    }

    // Copy .mo files to OUT_DIR so they can be found at runtime if needed
    let out_mo_dir = Path::new(&out_dir).join("locale");
    let _ = fs::create_dir_all(&out_mo_dir);

    if let Ok(entries) = fs::read_dir(po_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("mo") {
                let lang = path.file_stem().and_then(|s| s.to_str()).unwrap_or("unknown");
                let dest = out_mo_dir.join(lang).join("LC_MESSAGES").join("cadrocfile.mo");
                let _ = fs::create_dir_all(dest.parent().unwrap());
                let _ = fs::copy(&path, &dest);
            }
        }
    }
}
