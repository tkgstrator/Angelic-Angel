use crate::transaction::animation::{KEY_INDEX, PATH_D, SVG_ANIMATION};
use crate::transaction::discovery::{
    MODULE_IMPORT, ON_DEMAND_ENTRY, ON_DEMAND_INDEX, SCRIPT_SRC, SIGNER_PATH,
};
use crate::transaction::fixture;
use crate::transaction::{
    ADDITIONAL_RANDOM_NUMBER, Transaction, VERIFICATION_KEY, ondemand_file_hash,
};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};

#[test]
fn every_static_pattern_compiles() {
    for pattern in [
        &ON_DEMAND_INDEX,
        &ON_DEMAND_ENTRY,
        &KEY_INDEX,
        &VERIFICATION_KEY,
        &SVG_ANIMATION,
        &PATH_D,
        &SCRIPT_SRC,
        &MODULE_IMPORT,
        &SIGNER_PATH,
    ] {
        let _ = pattern.as_str();
    }
}

#[test]
fn id_layout_is_xor_key_then_masked_bytes() {
    let transaction = fixture::transaction();
    let id = transaction.generate_with("GET", "/1.1/account/settings.json", 1000, 0);
    let bytes = STANDARD_NO_PAD.decode(&id).unwrap();
    assert!(!id.contains('='));
    assert_eq!(bytes[0], 0);
    assert_eq!(bytes.len(), 1 + 70 + 4 + 16 + 1);
    assert_eq!(&bytes[1..71], transaction.key_bytes.as_slice());
    assert_eq!(&bytes[71..75], &1000i32.to_le_bytes());
    assert_eq!(bytes[bytes.len() - 1], ADDITIONAL_RANDOM_NUMBER);

    let masked = STANDARD_NO_PAD
        .decode(transaction.generate_with("GET", "/1.1/account/settings.json", 1000, 0x5a))
        .unwrap();
    assert_eq!(masked[0], 0x5a);
    for (plain, masked) in bytes[1..].iter().zip(&masked[1..]) {
        assert_eq!(plain ^ 0x5a, *masked);
    }
    assert_ne!(
        transaction.generate_with("POST", "/1.1/account/settings.json", 1000, 0),
        id
    );
    assert_ne!(
        transaction.generate_with("GET", "/i/api/1.1/friends/ids.json", 1000, 0),
        id
    );
    assert_ne!(
        transaction.generate_with("GET", "/1.1/account/settings.json", 1001, 0),
        id
    );
}

#[test]
fn relative_clock_is_after_the_x_epoch() {
    assert!(fixture::relative_now() > 0);
}

#[test]
fn hostile_input_is_an_error_not_a_panic() {
    let html = fixture::home_html();
    let js = fixture::ondemand_js();
    let huge_digits = "9".repeat(400);

    let cases: Vec<(String, String)> = vec![
        (String::new(), String::new()),
        (html.clone(), String::new()),
        (html.clone(), "(e[99], 16)".into()),
        (html.clone(), "(e[11], 16)(e[99], 16)".into()),
        (
            html.clone(),
            format!("(e[11], 16){}", "(e[12], 16)".repeat(400)),
        ),
        (
            html.replace("M 10,10 C ", &format!("M 10,10 C {huge_digits}")),
            js.clone(),
        ),
        (html.replace("content=\"", "content=\"A"), js.clone()),
        (
            html.replace("<path d=\"M 10,10 C", "<path d=\"M"),
            js.clone(),
        ),
        (
            format!(
                "<svg id=\"loading-x-anim-0\"><path d=\"a\"/><path d=\"\u{1F600}\u{1F600}\u{1F600}\u{1F600}\u{1F600}C 1\"/></svg>{html}"
            ),
            js.clone(),
        ),
        (html.replace("C", "CC"), js.clone()),
        (html.chars().take(html.len() / 2).collect(), js.clone()),
    ];
    for (html, js) in &cases {
        let _ = Transaction::new(html, js);
    }

    // 添字・バイト列・トークンを組み合わせた決定的なでたらめ入力
    let tokens = [
        "<svg id=\"loading-x-anim-1\">",
        "</svg>",
        "<path d=\"",
        "\"/>",
        " C ",
        ",",
        "12",
        "255",
        "9999999999999999999999",
        "M 10,10 C",
        "<meta name=\"twitter-site-verification\" content=\"",
        "QUJD",
        "AAAA",
        "\">",
        "(e[5], 16)",
        "(e[1], 16)",
        ",20113:\"ondemand.s\"",
        ",20113:\"abc\"",
    ];
    let mut state = 0x9e37_79b9_u32;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (state >> 16) as usize
    };
    for _ in 0..300 {
        let make = |n: &mut dyn FnMut() -> usize| {
            (0..(n() % 24))
                .map(|_| tokens[n() % tokens.len()])
                .collect::<String>()
        };
        let html = format!("{}{}", fixture::home_html(), make(&mut next));
        let js = format!("{}{}", fixture::ondemand_js(), make(&mut next));
        let _ = Transaction::new(&html, &js);
        let _ = Transaction::new(&make(&mut next), &make(&mut next));
        let _ = ondemand_file_hash(&make(&mut next));
    }
}
