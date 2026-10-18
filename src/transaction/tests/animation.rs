use crate::transaction::animation::{forgiving_base64_decode, skip_utf16_units};

#[test]
fn base64_decoding_follows_atob() {
    assert_eq!(forgiving_base64_decode("QUJD").unwrap(), b"ABC");
    assert_eq!(forgiving_base64_decode("QUI=").unwrap(), b"AB");
    assert_eq!(forgiving_base64_decode("QUI").unwrap(), b"AB");
    assert_eq!(forgiving_base64_decode(" QU\nJD\t").unwrap(), b"ABC");
    assert_eq!(forgiving_base64_decode("QQ==").unwrap(), b"A");
    assert_eq!(forgiving_base64_decode("").unwrap(), b"");
    assert!(forgiving_base64_decode("Q").is_none());
    assert!(forgiving_base64_decode("QUJDR").is_none());
    assert!(forgiving_base64_decode("QU=J").is_none());
    assert!(forgiving_base64_decode("QUI==").is_none());
    assert!(forgiving_base64_decode("QUJ-").is_none());
    assert!(forgiving_base64_decode("QUJ\u{3042}").is_none());
}

#[test]
fn utf16_skip_matches_substring() {
    assert_eq!(skip_utf16_units("M 10,10 CABC", 9), "ABC");
    assert_eq!(skip_utf16_units("short", 9), "");
    assert_eq!(skip_utf16_units("あいうえおかきくけこ", 9), "こ");
    assert_eq!(skip_utf16_units("12345678\u{1F600}x", 9), "x");
}
