use flate2::write::GzEncoder;
use flate2::Compression;
use std::env;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;

fn main() {
    let out = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    // 1. Memory and Linker Scripts
    File::create(out.join("memory.x"))
        .unwrap()
        .write_all(include_bytes!("memory.x"))
        .unwrap();
    println!("cargo:rustc-link-search={}", out.display());
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rustc-link-arg-bins=--nmagic");
    println!("cargo:rustc-link-arg-bins=-Tlink.x");
    println!("cargo:rustc-link-arg-bins=-Tdefmt.x");
    println!("cargo:rustc-link-arg-bins=-Tlink-rp.x");

    // 2. Web UI Compression
    println!("cargo:rerun-if-changed=html/index.html");
    let dest_path = out.join("index.html.gz");

    if let Ok(mut original_file) = File::open("html/index.html") {
        let mut contents = Vec::new();
        original_file.read_to_end(&mut contents).unwrap();
        let target_file = File::create(&dest_path).unwrap();
        let mut encoder = GzEncoder::new(target_file, Compression::best());
        encoder.write_all(&contents).unwrap();
        encoder.finish().unwrap();
    } else {
        File::create(&dest_path).unwrap().write_all(b"").unwrap();
    }
}
