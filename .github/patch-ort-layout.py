"""Bounded CI compatibility patch for ort-sys 2.0.0-rc.10 + Cargo 1.100.

Cargo's nested build output breaks the dependency's fixed ancestor count.
Only this exact version and exact preimage are accepted. No runtime code changes.
Remove when ort-sys is upgraded to a version supporting the nested layout.
"""
import os
from pathlib import Path

OLD = "\tlet out_dir = out_dir.ancestors().nth(3).unwrap();"
NEW = '''\tlet out_dir = out_dir.ancestors()
        .find(|p| p.file_name().is_some_and(|name| name == "build"))
        .and_then(Path::parent)
        .expect("ort-sys: expected Cargo build directory ancestor");'''
LOOP = '\tfor out_dir in [out_dir.to_path_buf(), out_dir.join("examples"), out_dir.join("deps")] {'
PATCHED_LOOP = LOOP + '\n        std::fs::create_dir_all(&out_dir).expect("ort-sys: create DLL output directory");'


def patch(path):
    source = path.read_text()
    if NEW in source and PATCHED_LOOP in source:
        return
    if source.count(OLD) != 1 or source.count(LOOP) != 1:
        raise RuntimeError(f"Unrecognized ort-sys copy layout in {path.name}; no patch applied")
    updated = source.replace(OLD, NEW).replace(LOOP, PATCHED_LOOP)
    path.write_text(updated)
    assert NEW in path.read_text() and PATCHED_LOOP in path.read_text()


if __name__ == "__main__":
    cargo = Path(os.environ.get("CARGO_HOME", str(Path.home() / ".cargo")))
    matches = list((cargo / "registry" / "src").glob("*/ort-sys-2.0.0-rc.10/build.rs"))
    if len(matches) != 1:
        raise SystemExit(f"Expected exactly one fetched ort-sys 2.0.0-rc.10; found {len(matches)}")
    patch(matches[0])
    print("Applied exact ort-sys Cargo-layout compatibility patch")
