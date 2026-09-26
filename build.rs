// `sqlx::migrate!()` embeds `migrations/` at compile time, but Cargo does not track that
// directory on its own. Without this, adding or editing a migration can leave a stale binary.
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
