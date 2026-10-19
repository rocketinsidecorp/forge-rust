//! Embeds every file under `assets/` in the binary with a content digest
//! (`$OUT_DIR/forge_assets.rs`, included by `src/adapters/http/views/mod.rs`).

fn main() {
    if let Err(failure) = forge_views::build::embed_assets_with_crate_path("assets", "forge::views")
    {
        eprintln!("asset embedding failed: {failure}");
        std::process::exit(1);
    }
}
