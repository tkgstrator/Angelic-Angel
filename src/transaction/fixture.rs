use super::*;
use base64::engine::general_purpose::STANDARD;

pub const ONDEMAND_HASH: &str = "c0ffee1";

fn key_bytes() -> Vec<u8> {
    (0..70u32).map(|i| ((i * 37 + 11) % 256) as u8).collect()
}

/// 合成したホーム HTML (旧 webpack の ondemand.s 対応表つき)。本物の X のデータは含まない。
pub fn home_html() -> String {
    home_html_with(true, "")
}

/// `legacy` なら ondemand.s の対応表を含め、`body_extra` を body の末尾に足す。
pub fn home_html_with(legacy: bool, body_extra: &str) -> String {
    let frames: String = (0..4u32)
        .map(|frame| {
            let rows: Vec<String> = (0..16u32)
                .map(|row| {
                    (0..11u32)
                        .map(|n| ((frame * 31 + row * 17 + n * 23 + 5) % 256).to_string())
                        .collect::<Vec<_>>()
                        .join(",")
                })
                .collect();
            format!(
                r#"<svg id="loading-x-anim-{frame}"><g><path d="M0 0L1 1"/><path d="M 10,10 C {}"/></g></svg>"#,
                rows.join(" C ")
            )
        })
        .collect();
    let manifest = if legacy {
        format!(r#"<script>{{1:"x",20113:"ondemand.s"}}{{1:"y",20113:"{ONDEMAND_HASH}"}}</script>"#)
    } else {
        String::new()
    };
    format!(
        r#"<html><head><meta name="twitter-site-verification" content="{}"></head><body>{manifest}{frames}{body_extra}</body></html>"#,
        STANDARD.encode(key_bytes())
    )
}

pub fn ondemand_js() -> String {
    "var r=(e[11], 16);var a=(e[10], 16);var b=(e[20], 16);var c=(e[30], 16);".to_string()
}

pub fn transaction() -> Transaction {
    Transaction::new(&home_html(), &ondemand_js()).expect("合成フィクスチャは有効")
}

/// 受信した ID から (xor, 相対秒) を取り出す。
pub fn decode_id(transaction: &Transaction, id: &str) -> (u8, i64) {
    let bytes = STANDARD_NO_PAD.decode(id).expect("ID は base64");
    let xor = bytes[0];
    let offset = 1 + transaction.key_bytes.len();
    let time: Vec<u8> = bytes[offset..offset + 4].iter().map(|b| b ^ xor).collect();
    (xor, i64::from(i32::from_le_bytes(time.try_into().unwrap())))
}

pub fn expected_id(transaction: &Transaction, method: &str, path: &str, id: &str) -> String {
    let (xor, time) = decode_id(transaction, id);
    transaction.generate_with(method, path, time, xor)
}

pub fn relative_now() -> i64 {
    relative_seconds_now()
}
