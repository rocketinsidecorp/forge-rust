//! The serde error type below the public `QueryError`.

use super::{DeError, QueryErrorKind};

#[test]
fn a_decoder_error_displays_its_detail() {
    let error = DeError::new(QueryErrorKind::InvalidValue, Some("limit"), "bad digit");
    assert_eq!(error.to_string(), "bad digit");
}
