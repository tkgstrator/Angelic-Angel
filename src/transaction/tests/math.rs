use crate::transaction::math::{custom_round, float_to_hex, js_round};

#[test]
fn js_rounding_matches_math_round() {
    assert_eq!(js_round(2.5), 3.0);
    assert_eq!(js_round(-2.5), -2.0);
    assert_eq!(js_round(0.49999999999999994), 0.0);
    assert_eq!(js_round(-0.2).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(js_round(-0.5).to_bits(), (-0.0_f64).to_bits());
    assert_eq!(js_round(-0.51), -1.0);
    assert_eq!(js_round(4503599627370497.0), 4503599627370497.0);
    assert!(js_round(f64::NAN).is_nan());
    assert_eq!(custom_round(7.5), 8.0);
    assert_eq!(custom_round(7.4), 7.0);
    assert_eq!(custom_round(0.0), 0.0);
}

#[test]
fn float_to_hex_matches_upstream_digits() {
    assert_eq!(float_to_hex(0.0), "");
    assert_eq!(float_to_hex(1.0), "1");
    assert_eq!(float_to_hex(0.5), ".8");
    assert_eq!(float_to_hex(20.75), "14.C");
    assert_eq!(float_to_hex(0.25), ".4");
    assert_eq!(float_to_hex(0.57), ".91EB851EB851E8");
    assert_eq!(float_to_hex(0.07), ".11EB851EB851EC");
}
