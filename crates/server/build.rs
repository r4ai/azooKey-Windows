use std::{env, path::PathBuf};

fn main() {
    // build_swift places the import library at the workspace root. Point the
    // linker at that exact directory instead of a crate-local target folder.
    let project_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let workspace_dir = project_dir.join("../..");
    println!("cargo:rustc-link-search=native={}", workspace_dir.display());
    println!("cargo:rustc-link-lib=azookey-server");
    println!(
        "cargo:rerun-if-changed={}",
        workspace_dir.join("azookey-server.lib").display()
    );
}
