//! Force Cargo to rebuild the embedded migration set when a migration file is
//! added, removed or renamed. `sqlx::migrate!` embeds the files known at compile
//! time; without this, an incremental build can keep serving the old set.

fn main() {
    println!("cargo:rerun-if-changed=../../migrations");
    println!("cargo:rerun-if-changed=build.rs");
}
