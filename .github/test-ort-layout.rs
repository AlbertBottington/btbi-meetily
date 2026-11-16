// Verify the same path resolution as patch-ort-layout.py on both Cargo layouts.
use std::path::{Path, PathBuf};
fn output_root(out: &Path) -> PathBuf {
    out.ancestors()
        .find(|p| p.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .expect("Cargo build ancestor")
        .to_path_buf()
}
#[test]
fn old_and_nested_layout_resolve_to_profile() {
    for input in ["target/debug/build/ort-sys-hash/out", "target/debug/build/ort-sys/hash/out"] {
        assert_eq!(output_root(Path::new(input)), PathBuf::from("target/debug"));
    }
    assert_eq!(output_root(Path::new("target/x86_64-pc-windows-msvc/release/build/ort-sys/hash/out")), PathBuf::from("target/x86_64-pc-windows-msvc/release"));
}
#[test]
#[should_panic]
fn unexpected_layout_fails_instead_of_copying_elsewhere() {
    output_root(Path::new("unrecognized/out"));
}
