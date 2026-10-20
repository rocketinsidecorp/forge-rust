//! Embeds every file under `assets/` in the binary with a content digest
//! (`$OUT_DIR/forge_assets.rs`, included by `src/adapters/http/views/mod.rs`)
//! and the translations of `config/locales` (`$OUT_DIR/forge_locales.rs`,
//! included by `src/adapters/http/views/locales.rs`).

fn main() {
    if let Err(failure) = forge_views::build::embed_assets_with_crate_path("assets", "forge::views")
    {
        eprintln!("asset embedding failed: {failure}");
        std::process::exit(1);
    }
    if let Err(failure) = forge_views::build::embed_locales("config/locales") {
        eprintln!("translation embedding failed: {failure}");
        std::process::exit(1);
    }
}
