//! Vendored, integrity-pinned JavaScript served through the import map.
//!
//! Files live unmodified under `vendor/`; their provenance (npm tarball,
//! integrity, license) is recorded in each directory's `README.md`.
//!
//! [`HOTWIRE_ASSETS`] serves both modules from an application's
//! [`AssetSet`](crate::AssetSet), next to the application's own assets:
//!
//! ```
//! use forge_views::{AssetSet, ImportMap, vendor};
//!
//! let assets = AssetSet::from_tables(&[vendor::HOTWIRE_ASSETS])?;
//! let map = ImportMap::builder()
//!     .pin("@hotwired/turbo", vendor::TURBO_ASSET)
//!     .pin("@hotwired/stimulus", vendor::STIMULUS_ASSET)
//!     .build(&assets)?;
//! assert!(map.json().contains("/assets/vendor/turbo-b9d35d123a07614f.js"));
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```

use crate::AssetEntry;

/// Stimulus 3.2.2 ES module build (`@hotwired/stimulus`, MIT).
pub const STIMULUS_JS: &str = include_str!("../vendor/stimulus/stimulus.js");

/// Version of [`STIMULUS_JS`].
pub const STIMULUS_VERSION: &str = "3.2.2";

/// Lower-case hex SHA-256 of [`STIMULUS_JS`], pinned when the file was vendored.
pub const STIMULUS_SHA256: &str =
    "23deecdac6f36c08e8f39fbed6b27f60c850cacad3a34eb71194462b7b1647f3";

/// Turbo 8.0.23 ES module build (`@hotwired/turbo`, MIT).
pub const TURBO_JS: &str = include_str!("../vendor/turbo/turbo.js");

/// Version of [`TURBO_JS`].
pub const TURBO_VERSION: &str = "8.0.23";

/// Lower-case hex SHA-256 of [`TURBO_JS`], pinned when the file was vendored.
pub const TURBO_SHA256: &str = "b9d35d123a07614f55eaaf993f74d687a503ae41ba50ef835aafa18dbb265a13";

/// Logical asset name of [`TURBO_JS`] in [`HOTWIRE_ASSETS`].
pub const TURBO_ASSET: &str = "vendor/turbo.js";

/// Logical asset name of [`STIMULUS_JS`] in [`HOTWIRE_ASSETS`].
pub const STIMULUS_ASSET: &str = "vendor/stimulus.js";

/// Turbo and Stimulus as embedded, digested assets. The digests are the
/// pinned [`TURBO_SHA256`] and [`STIMULUS_SHA256`]; `AssetSet` re-checks that
/// each digested name matches its digest at startup.
pub static HOTWIRE_ASSETS: &[AssetEntry] = &[
    AssetEntry::new(
        STIMULUS_ASSET,
        "vendor/stimulus-23deecdac6f36c08.js",
        STIMULUS_SHA256,
        STIMULUS_JS.as_bytes(),
    ),
    AssetEntry::new(
        TURBO_ASSET,
        "vendor/turbo-b9d35d123a07614f.js",
        TURBO_SHA256,
        TURBO_JS.as_bytes(),
    ),
];

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;

    fn sha256_hex(bytes: &[u8]) -> String {
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    #[test]
    fn vendored_turbo_matches_its_pinned_digest() {
        assert_eq!(
            sha256_hex(TURBO_JS.as_bytes()),
            TURBO_SHA256,
            "vendor/turbo/turbo.js was modified"
        );
    }

    #[test]
    fn hotwire_assets_are_a_valid_digested_table() {
        let assets = crate::AssetSet::new(HOTWIRE_ASSETS).expect("valid table");
        assert_eq!(assets.len(), 2);
        for entry in HOTWIRE_ASSETS {
            assert_eq!(
                sha256_hex(entry.bytes()),
                entry.digest(),
                "{}",
                entry.logical()
            );
        }
        assert_eq!(
            assets.path(TURBO_ASSET),
            Some("/assets/vendor/turbo-b9d35d123a07614f.js")
        );
        assert_eq!(
            assets.path(STIMULUS_ASSET),
            Some("/assets/vendor/stimulus-23deecdac6f36c08.js")
        );
    }

    #[test]
    fn vendored_stimulus_matches_its_pinned_digest() {
        let digest = Sha256::digest(STIMULUS_JS.as_bytes());
        let hex: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(
            hex, STIMULUS_SHA256,
            "vendor/stimulus/stimulus.js was modified"
        );
        assert!(
            STIMULUS_JS.contains("export {"),
            "expected an ES module build"
        );
    }
}
