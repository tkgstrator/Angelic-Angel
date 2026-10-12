//! x-client-transaction-id ジェネレータ。qtmleap/x-transaction (MIT, (c) 2026 QuantumLeap) の
//! `src/transaction.ts` の移植で、ライセンス原文は `LICENSE.x-transaction` に置く。

use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use regex::Regex;
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const ADDITIONAL_RANDOM_NUMBER: u8 = 3;
const DEFAULT_KEYWORD: &str = "obfiowerehiring";
const TWITTER_EPOCH_MS: i64 = 1_682_924_400_000;
const TOTAL_ANIMATION_TIME: f64 = 4096.0;
/// アニメーション行に必要な数値の数 (色 6 + 回転 1 + ベジェ 4)。
const FRAME_VALUES: usize = 11;
/// 16 進変換ループの上限。有限の入力では 14 桁程度で止まる。
const HEX_LOOP_LIMIT: usize = 64;

/// JavaScript の `\s` と同じ空白文字。
const WS: &str = r"[\t\n\x0B\x0C\r \u{A0}\u{1680}\u{2000}-\u{200A}\u{2028}\u{2029}\u{202F}\u{205F}\u{3000}\u{FEFF}]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum TransactionError {
    #[error("twitter-site-verification key is missing or invalid")]
    VerificationKey,
    #[error("key byte indices are missing")]
    KeyIndices,
    #[error("animation frame is missing or malformed")]
    AnimationFrame,
    #[error("animation values are not finite")]
    NonFinite,
}

fn compile(pattern: &str) -> Regex {
    Regex::new(pattern).expect("静的パターンは常にコンパイルできる")
}

static ON_DEMAND_INDEX: LazyLock<Regex> =
    LazyLock::new(|| compile(r#",([0-9]+):["']ondemand\.s["']"#));
static ON_DEMAND_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| compile(r#",([0-9]+):["']([0-9a-f]+)["']"#));
static KEY_INDEX: LazyLock<Regex> =
    LazyLock::new(|| compile(&format!(r"\([A-Za-z0-9_]\[([0-9][0-9]?)\],{WS}*16\)")));
static VERIFICATION_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r#"<(?i-u:meta){WS}+(?i-u:name)=["'](?i-u:twitter-site-verification)["']{WS}+(?i-u:content)=["']([^"']+)["']"#
    ))
});
static SVG_ANIMATION: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"<(?i-u:svg)[^>]*(?i-u:id)=["'](?i-u:loading-x-anim-)[0-9]+["'][^>]*>(?s:.)*?</(?i-u:svg)>"#,
    )
});
/// 上流 discovery.ts の `<script src>` 抽出。ASCII の空白と境界だけを使う。
static SCRIPT_SRC: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"<(?i-u:script)(?-u:\b)[^>]*(?-u:\b)(?i-u:src)(?-u:\s)*=(?-u:\s)*["']([^"']+)["'][^>]*>"#,
    )
});
/// 上流 discovery.ts の import / dynamic import / re-export 抽出。
static MODULE_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?-u:\b)(?:import(?-u:\s)*\((?-u:\s)*|import(?-u:\s)*(?:[^"'`;]*?(?-u:\b)from(?-u:\s)*)?|export(?-u:\s)+[^;]*?(?-u:\b)from(?-u:\s)*)["'`]([^"'`]+)["'`]"#,
    )
});
static SIGNER_PATH: LazyLock<Regex> = LazyLock::new(|| compile(r"/sign[.-][^/]*\.js(?:\?|$)"));
static PATH_D: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"<(?i-u:path)[^>]*(?i-u:d)=["']([^"']+)["']"#));

// ---- JavaScript 互換の数値処理 ---------------------------------------------------------------

/// `Math.sign`。±0 は符号を保ち、NaN は NaN を返す。
fn js_sign(x: f64) -> f64 {
    if x.is_nan() || x == 0.0 {
        x
    } else if x > 0.0 {
        1.0
    } else {
        -1.0
    }
}

/// `Math.round`。半分は +∞ 側へ丸め、-0.5 以上 0 未満は -0 になる。
fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let floor = x.floor();
    let rounded = if x - floor >= 0.5 { floor + 1.0 } else { floor };
    if rounded == 0.0 && x < 0.0 {
        -0.0
    } else {
        rounded
    }
}

/// 上流の `customRound`。
fn custom_round(num: f64) -> f64 {
    let x = num.floor();
    if num - x >= 0.5 {
        num.ceil()
    } else {
        js_sign(num) * x.abs()
    }
}

fn is_odd(index: usize) -> f64 {
    if index % 2 == 1 { -1.0 } else { 0.0 }
}

fn interpolate(from: f64, to: f64, factor: f64) -> f64 {
    from * (1.0 - factor) + to * factor
}

fn hex_digit(n: f64) -> char {
    let n = n as u8;
    if n > 9 {
        char::from(n.saturating_add(55))
    } else {
        char::from(b'0' + n)
    }
}

/// 上流の `floatToHex`。整数部と小数部を 16 進で表す。
fn float_to_hex(x: f64) -> String {
    let mut int_digits = Vec::new();
    let mut quotient = x.trunc();
    let mut current = x;
    let mut fraction = x - quotient;

    for _ in 0..HEX_LOOP_LIMIT {
        if quotient <= 0.0 {
            break;
        }
        let new_quotient = (current / 16.0).trunc();
        let remainder = (current - new_quotient * 16.0).trunc();
        int_digits.push(hex_digit(remainder));
        current = new_quotient;
        quotient = new_quotient;
    }
    let mut result: String = int_digits.into_iter().rev().collect();

    if fraction == 0.0 {
        return result;
    }
    result.push('.');
    for _ in 0..HEX_LOOP_LIMIT {
        if fraction <= 0.0 {
            break;
        }
        fraction *= 16.0;
        let integer = fraction.trunc();
        fraction -= integer;
        result.push(hex_digit(integer));
    }
    result
}

// ---- Cubic Bezier ----------------------------------------------------------------------------

struct Cubic([f64; 4]);

impl Cubic {
    fn value(&self, time: f64) -> f64 {
        if time <= 0.0 {
            self.start_gradient() * time
        } else if time >= 1.0 {
            1.0 + self.end_gradient() * (time - 1.0)
        } else {
            self.binary_search(time)
        }
    }

    fn start_gradient(&self) -> f64 {
        let c = &self.0;
        if c[0] > 0.0 {
            c[1] / c[0]
        } else if c[1] == 0.0 && c[2] > 0.0 {
            c[3] / c[2]
        } else {
            0.0
        }
    }

    fn end_gradient(&self) -> f64 {
        let c = &self.0;
        if c[2] < 1.0 {
            (c[3] - 1.0) / (c[2] - 1.0)
        } else if c[2] == 1.0 && c[0] < 1.0 {
            (c[1] - 1.0) / (c[0] - 1.0)
        } else {
            0.0
        }
    }

    fn binary_search(&self, time: f64) -> f64 {
        let c = &self.0;
        let (mut start, mut end, mut mid) = (0.0_f64, 1.0_f64, 0.0_f64);
        for _ in 0..100 {
            if start >= end {
                break;
            }
            mid = (start + end) / 2.0;
            let x_est = Self::calculate(c[0], c[2], mid);
            if (time - x_est).abs() < 0.00001 {
                return Self::calculate(c[1], c[3], mid);
            }
            if x_est < time {
                start = mid;
            } else {
                end = mid;
            }
        }
        Self::calculate(c[1], c[3], mid)
    }

    fn calculate(a: f64, b: f64, m: f64) -> f64 {
        let one_minus_m = 1.0 - m;
        3.0 * a * one_minus_m * one_minus_m * m + 3.0 * b * one_minus_m * m * m + m * m * m
    }
}

// ---- HTML / JS の解析 ------------------------------------------------------------------------

/// ホーム HTML から ondemand.s ファイルのハッシュを取り出す (2 段階の対応表を引く)。
pub fn ondemand_file_hash(home_html: &str) -> Option<&str> {
    let index = ON_DEMAND_INDEX.captures(home_html)?.get(1)?.as_str();
    ON_DEMAND_ENTRY
        .captures_iter(home_html)
        .find(|caps| caps.get(1).is_some_and(|m| m.as_str() == index))
        .and_then(|caps| caps.get(2))
        .map(|m| m.as_str())
}

/// 取得してよい署名モジュールの URL か。`asset_base` と同一オリジンで、`/x-web/` 配下の `.js` だけを通す。
pub fn asset_url(value: &str, base: &Url, asset_base: &Url) -> Option<Url> {
    if value.contains("${") {
        return None;
    }
    let url = base.join(value).ok()?;
    let allowed = url.origin() == asset_base.origin()
        && url.username().is_empty()
        && url.password().is_none()
        && url.path().starts_with("/x-web/")
        && url.path().ends_with(".js");
    allowed.then_some(url)
}

/// ホーム HTML の `<script src>` のうち、探索の起点にする `entry-client` モジュール。
pub fn entry_script_urls(html: &str, base: &Url, asset_base: &Url) -> Vec<Url> {
    SCRIPT_SRC
        .captures_iter(html)
        .filter_map(|caps| asset_url(caps.get(1)?.as_str(), base, asset_base))
        .filter(|url| url.as_str().contains("entry-client"))
        .collect()
}

/// モジュールが import / re-export する署名候補 URL。重複は初出の順に畳む。
pub fn module_import_urls(source: &str, base: &Url, asset_base: &Url) -> Vec<Url> {
    let mut urls: Vec<Url> = Vec::new();
    for caps in MODULE_IMPORT.captures_iter(source) {
        let Some(url) = caps
            .get(1)
            .and_then(|value| asset_url(value.as_str(), base, asset_base))
        else {
            continue;
        };
        if !urls.contains(&url) {
            urls.push(url);
        }
    }
    urls
}

/// 現行の署名モジュール (`sign.o-*.js`) の URL か。
pub fn is_signer_url(url: &Url) -> bool {
    SIGNER_PATH.is_match(url.as_str())
}

/// 探索の優先度。小さいほど先に取得する。
pub fn module_priority(url: &Url) -> u8 {
    let text = url.as_str();
    if text.contains("/sign.") || text.contains("/sign-") {
        0
    } else if text.contains("sentry") || text.contains("transaction") {
        1
    } else {
        2
    }
}

/// ondemand.s から (行を選ぶ添字, 残りのキーバイト添字) を取り出す。
fn parse_key_byte_indices(ondemand_js: &str) -> Result<(usize, Vec<usize>), TransactionError> {
    let mut indices = KEY_INDEX
        .captures_iter(ondemand_js)
        .filter_map(|caps| caps.get(1)?.as_str().parse::<usize>().ok());
    let row_index = indices.next().ok_or(TransactionError::KeyIndices)?;
    Ok((row_index, indices.collect()))
}

/// 各アニメーション SVG の 2 本目の path から `d` 属性を取り出す。
fn parse_animation_frames(home_html: &str) -> Vec<&str> {
    SVG_ANIMATION
        .find_iter(home_html)
        .filter_map(|svg| {
            PATH_D
                .captures_iter(svg.as_str())
                .nth(1)
                .and_then(|caps| caps.get(1))
                .map(|m| m.as_str())
        })
        .collect()
}

/// JavaScript の `substring(9)` (UTF-16 単位) と同じ位置から後ろを返す。
fn skip_utf16_units(text: &str, units: usize) -> &str {
    let mut consumed = 0;
    for (offset, ch) in text.char_indices() {
        if consumed >= units {
            return &text[offset..];
        }
        consumed += ch.len_utf16();
    }
    ""
}

/// path データを数値の 2 次元配列にする。10 進数字の連なりだけを数として読む。
fn parse_path_rows(path_data: &str) -> Result<Vec<Vec<f64>>, TransactionError> {
    skip_utf16_units(path_data, 9)
        .split('C')
        .map(|command| {
            command
                .split(|c: char| !c.is_ascii_digit())
                .filter(|run| !run.is_empty())
                .map(|run| match run.parse::<f64>() {
                    Ok(value) if value.is_finite() => Ok(value),
                    _ => Err(TransactionError::NonFinite),
                })
                .collect()
        })
        .collect()
}

/// `atob` と同じ forgiving-base64。末尾の端数ビットは捨てる。
fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
    let mut data: Vec<u8> = input
        .bytes()
        .filter(|b| !matches!(b, b'\t' | b'\n' | 0x0C | b'\r' | b' '))
        .collect();
    if data.len().is_multiple_of(4) {
        for _ in 0..2 {
            if data.last() == Some(&b'=') {
                data.pop();
            }
        }
    }
    if data.len() % 4 == 1 {
        return None;
    }
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let (mut buffer, mut bits) = (0u32, 0u32);
    for byte in data {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            _ => return None,
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buffer >> bits) & 0xff) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

// ---- Transaction -----------------------------------------------------------------------------

/// ホーム HTML と ondemand.s から作る ID ジェネレータ。作成後は何度でも使い回せる。
pub struct Transaction {
    key_bytes: Vec<u8>,
    animation_key: String,
}

impl Transaction {
    pub fn new(home_html: &str, ondemand_js: &str) -> Result<Self, TransactionError> {
        let (row_index, key_indices) = parse_key_byte_indices(ondemand_js)?;
        let key = VERIFICATION_KEY
            .captures(home_html)
            .and_then(|caps| caps.get(1))
            .map(|m| m.as_str())
            .ok_or(TransactionError::VerificationKey)?;
        let key_bytes = forgiving_base64_decode(key).ok_or(TransactionError::VerificationKey)?;
        let animation_key = animation_key(&key_bytes, row_index, &key_indices, home_html)?;
        Ok(Self {
            key_bytes,
            animation_key,
        })
    }

    /// 現在時刻で `method` と `path` (クエリを除く) の ID を作る。
    pub fn generate(&self, method: &str, path: &str) -> String {
        self.generate_with(method, path, relative_seconds_now(), rand::random())
    }

    fn generate_with(&self, method: &str, path: &str, time: i64, xor: u8) -> String {
        let hash_input = format!(
            "{method}!{path}!{time}{DEFAULT_KEYWORD}{}",
            self.animation_key
        );
        let digest = Sha256::digest(hash_input.as_bytes());
        // JavaScript の `>>` と同じく 32bit に丸めたうえでリトルエンディアンにする
        let time_bytes = (time as i32).to_le_bytes();

        let mut out = Vec::with_capacity(self.key_bytes.len() + 22);
        out.push(xor);
        out.extend(
            self.key_bytes
                .iter()
                .chain(&time_bytes)
                .chain(&digest[..16])
                .chain(&[ADDITIONAL_RANDOM_NUMBER])
                .map(|byte| byte ^ xor),
        );
        STANDARD_NO_PAD.encode(out)
    }
}

/// X のエポック (2023-05-01) からの経過秒。エポック前は負になる。
fn relative_seconds_now() -> i64 {
    let now_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(elapsed) => i64::try_from(elapsed.as_millis()).unwrap_or(i64::MAX),
        Err(before) => i64::try_from(before.duration().as_millis()).map_or(i64::MIN, |ms| -ms),
    };
    now_ms.saturating_sub(TWITTER_EPOCH_MS).div_euclid(1000)
}

fn animation_key(
    key_bytes: &[u8],
    row_index: usize,
    key_indices: &[usize],
    home_html: &str,
) -> Result<String, TransactionError> {
    let byte = |index: usize| {
        key_bytes
            .get(index)
            .copied()
            .ok_or(TransactionError::AnimationFrame)
    };

    let row = usize::from(byte(row_index)? % 16);
    let frame_index = usize::from(byte(5)? % 4);

    let mut raw_frame_time = 1.0_f64;
    for &index in key_indices {
        raw_frame_time *= f64::from(byte(index)? % 16);
    }
    let frame_time = custom_round(raw_frame_time / 10.0) * 10.0;
    let target_time = frame_time / TOTAL_ANIMATION_TIME;
    if !target_time.is_finite() {
        return Err(TransactionError::NonFinite);
    }

    let frames = parse_animation_frames(home_html);
    let path_data = frames
        .get(frame_index)
        .ok_or(TransactionError::AnimationFrame)?;
    let rows = parse_path_rows(path_data)?;
    let frame_row = rows.get(row).ok_or(TransactionError::AnimationFrame)?;
    animate(frame_row, target_time)
}

/// `(value * (max - min)) / 255 + min` を丸めて返す。
fn solve(value: f64, min: f64, max: f64, rounding: bool) -> f64 {
    let result = (value * (max - min)) / 255.0 + min;
    if rounding {
        result.floor()
    } else {
        js_round(result * 100.0) / 100.0
    }
}

fn animate(frames: &[f64], target_time: f64) -> Result<String, TransactionError> {
    if frames.len() < FRAME_VALUES {
        return Err(TransactionError::AnimationFrame);
    }
    let from_color = [frames[0], frames[1], frames[2]];
    let to_color = [frames[3], frames[4], frames[5]];
    let to_rotation = solve(frames[6], 60.0, 360.0, true);

    let mut curves = [0.0; 4];
    for (index, curve) in curves.iter_mut().enumerate() {
        *curve = solve(frames[7 + index], is_odd(index), 1.0, false);
    }
    let factor = Cubic(curves).value(target_time);

    let color = [0, 1, 2].map(|i| interpolate(from_color[i], to_color[i], factor));
    let rotation = interpolate(0.0, to_rotation, factor);
    if !factor.is_finite() || !rotation.is_finite() || color.iter().any(|v| !v.is_finite()) {
        return Err(TransactionError::NonFinite);
    }
    let color = color.map(|v| v.clamp(0.0, 255.0));

    let rad = (rotation * std::f64::consts::PI) / 180.0;
    let matrix = [rad.cos(), -rad.sin(), rad.sin(), rad.cos()];
    Ok(build_animation_key(color, matrix))
}

fn build_animation_key(color: [f64; 3], matrix: [f64; 4]) -> String {
    let mut parts: Vec<String> = color
        .iter()
        .map(|&c| format!("{:x}", js_round(c) as u32))
        .collect();
    for value in matrix {
        let rounded = (js_round(value * 100.0) / 100.0).abs();
        let hex = float_to_hex(rounded);
        parts.push(if hex.starts_with('.') {
            format!("0{hex}").to_ascii_lowercase()
        } else if hex.is_empty() {
            "0".to_string()
        } else {
            hex.to_ascii_lowercase()
        });
    }
    parts.push("0".into());
    parts.push("0".into());
    parts.concat().replace(['.', '-'], "")
}

#[cfg(test)]
pub(crate) mod fixture {
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
            format!(
                r#"<script>{{1:"x",20113:"ondemand.s"}}{{1:"y",20113:"{ONDEMAND_HASH}"}}</script>"#
            )
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct IdVector {
        method: String,
        path: String,
        time: i64,
        xor: u8,
        id: String,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Case {
        name: String,
        kind: String,
        html: String,
        ondemand: String,
        ondemand_url: Option<String>,
        animation_key: Option<String>,
        ids: Vec<IdVector>,
    }

    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Counts {
        transaction: usize,
        throws: usize,
        ondemand_url: usize,
        ids: usize,
    }

    #[derive(Deserialize)]
    struct Vectors {
        counts: Counts,
        cases: Vec<Case>,
    }

    fn upstream_url(hash: &str) -> String {
        format!("https://abs.twimg.com/responsive-web/client-web/ondemand.s.{hash}a.js")
    }

    #[test]
    fn matches_the_upstream_typescript_vectors() {
        let vectors: Vectors =
            serde_json::from_str(include_str!("../scripts/x-transaction-vectors.json")).unwrap();
        let (mut transactions, mut throws, mut urls, mut ids) = (0, 0, 0, 0);
        for case in &vectors.cases {
            assert_eq!(
                ondemand_file_hash(&case.html).map(upstream_url),
                case.ondemand_url,
                "{}: ondemand URL",
                case.name
            );
            match case.kind.as_str() {
                "transaction" => {
                    transactions += 1;
                    let transaction = Transaction::new(&case.html, &case.ondemand)
                        .unwrap_or_else(|e| panic!("{}: {e}", case.name));
                    assert_eq!(
                        Some(&transaction.animation_key),
                        case.animation_key.as_ref(),
                        "{}: animation key",
                        case.name
                    );
                    for vector in &case.ids {
                        ids += 1;
                        assert_eq!(
                            transaction.generate_with(
                                &vector.method,
                                &vector.path,
                                vector.time,
                                vector.xor
                            ),
                            vector.id,
                            "{}: {} {} t={} xor={}",
                            case.name,
                            vector.method,
                            vector.path,
                            vector.time,
                            vector.xor
                        );
                    }
                },
                "throws" => {
                    throws += 1;
                    assert!(
                        Transaction::new(&case.html, &case.ondemand).is_err(),
                        "{}: 上流が失敗する入力を受理した",
                        case.name
                    );
                },
                "ondemand-url" => urls += 1,
                other => panic!("{}: 未知の kind {other}", case.name),
            }
        }
        assert_eq!(transactions, vectors.counts.transaction);
        assert_eq!(throws, vectors.counts.throws);
        assert_eq!(urls, vectors.counts.ondemand_url);
        assert_eq!(ids, vectors.counts.ids);
        assert!(transactions >= 20 && throws >= 5 && urls >= 5 && ids >= 100);
    }

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

    fn abs(path: &str) -> Url {
        Url::parse("https://abs.twimg.com")
            .unwrap()
            .join(path)
            .unwrap()
    }

    fn import_paths(source: &str) -> Vec<String> {
        let base = abs("/x-web/x-web/entry-client-logged-out-abc.js");
        let asset_base = abs("/");
        module_import_urls(source, &base, &asset_base)
            .iter()
            .map(|url| url.as_str().replace("https://abs.twimg.com", ""))
            .collect()
    }

    /// 期待値は PR #5 (c6400ea) の discovery.ts に同じ入力を与えて記録した取得順。
    #[test]
    fn module_imports_match_the_upstream_discovery_order() {
        let cases: [(&str, &[&str]); 8] = [
            (
                r#"import a from"./a.js";import{b}from './b.js';export*from"./c.js";export{d}from'./d.js';import"./e.js";import("./f.js");import
(
"./g.js");import(`./h.js`);import x,{y}from"./i.js";const important="./nope.js";import q from"./a.js";import.meta.url;"#,
                &[
                    "/x-web/x-web/a.js",
                    "/x-web/x-web/b.js",
                    "/x-web/x-web/e.js",
                    "/x-web/x-web/f.js",
                    "/x-web/x-web/g.js",
                    "/x-web/x-web/h.js",
                    "/x-web/x-web/i.js",
                ],
            ),
            (
                r#"import"./a.js?x=1#h";import"../up.js";import"/x-web/abs.js";import"//abs.twimg.com/x-web/schemeless.js";import"./data.json";import"https://abs.twimg.com:443/x-web/port443.js";import"https://abs.twimg.com:8443/x-web/port.js";import"https://user@abs.twimg.com/x-web/user.js";import"HTTPS://ABS.TWIMG.COM/x-web/upper.js";import"./a/../norm.js";import"http://abs.twimg.com/x-web/http.js";import"https://abs.twimg.com/other/outside.js";"#,
                &[
                    "/x-web/x-web/a.js?x=1#h",
                    "/x-web/up.js",
                    "/x-web/abs.js",
                    "/x-web/schemeless.js",
                    "/x-web/port443.js",
                    "/x-web/upper.js",
                    "/x-web/x-web/norm.js",
                ],
            ),
            (
                r#"import(`./${x}.js`);import(`./ok.js`);import"./a${b}.js";"#,
                &["/x-web/x-web/ok.js"],
            ),
            (
                r#"import a from `./t1.js`;export * from './t2.js';export{x as y} from"./t3.js";import{z}from"./t4.js""#,
                &[
                    "/x-web/x-web/t1.js",
                    "/x-web/x-web/t2.js",
                    "/x-web/x-web/t4.js",
                ],
            ),
            (
                r#"let from="./nope.js";import x;import("./real.js");export default from;import{a}from"./b.js""#,
                &["/x-web/x-web/real.js", "/x-web/x-web/b.js"],
            ),
            (
                "import {\n  a,\n  b\n} from\n  \"./ml.js\";\nexport {\n  c\n} from\n\"./ml2.js\";",
                &["/x-web/x-web/ml.js", "/x-web/x-web/ml2.js"],
            ),
            (
                r#"import"./a.js";import"./a.js";import"./b.js";import"./a.js""#,
                &["/x-web/x-web/a.js", "/x-web/x-web/b.js"],
            ),
            ("", &[]),
        ];
        for (source, expected) in cases {
            assert_eq!(import_paths(source), expected, "{source}");
        }
    }

    /// 期待値は merge 後の上流 discovery.ts (85197cf) に同じ HTML を与えて記録した最初の取得先。
    #[test]
    fn entry_scripts_match_the_upstream_html_variants() {
        let home = Url::parse("https://x.com/").unwrap();
        let asset_base = abs("/");
        let e = "https://abs.twimg.com/x-web/x-web/entry-client-logged-out-abc.js";
        let second = "https://abs.twimg.com/x-web/x-web/entry-client-second.js";
        let cases: Vec<(String, Vec<&str>)> =
            vec![
            (format!(r#"<script type="module" src="{e}"></script>"#), vec![e]),
            (format!("<SCRIPT TYPE=\"module\" SRC='{e}'></SCRIPT>"), vec![e]),
            (format!(r#"<script src  =  "{e}" async></script>"#), vec![e]),
            (format!(r#"<script data-src="{e}"></script>"#), vec![e]),
            (format!(r#"<scripts src="{e}"></scripts>"#), vec![]),
            (format!("<script src={e}></script>"), vec![]),
            (r#"<script src="/x-web/x-web/entry-client-rel.js"></script>"#.into(), vec![]),
            (
                r#"<script src="x-web/x-web/entry-client-dot.js"></script>"#.into(),
                vec![],
            ),
            (
                r#"<script src="https://abs.twimg.com/x-web/x-web/other-abc.js"></script>"#.into(),
                vec![],
            ),
            (
                r#"<script src="https://evil.example/x-web/entry-client-x.js"></script>"#.into(),
                vec![],
            ),
            (
                r#"<script src="http://abs.twimg.com/x-web/x-web/entry-client-http.js"></script>"#
                    .into(),
                vec![],
            ),
            (
                r#"<script src="https://abs.twimg.com/other/entry-client-x.js"></script>"#.into(),
                vec![],
            ),
            (
                format!(r#"<script src="{e}"></script><script src="{second}"></script>"#),
                vec![e, second],
            ),
            (
                r#"<script src="https://abs.twimg.com/x-web/x-web/a.js?entry-client"></script>"#
                    .into(),
                vec!["https://abs.twimg.com/x-web/x-web/a.js?entry-client"],
            ),
            (
                format!("<script\n  type=\"module\"\n  src=\"{e}\"\n></script>"),
                vec![e],
            ),
            (format!(r#"<script>var s="src='{e}'"</script>"#), vec![]),
        ];
        for (html, expected) in cases {
            let urls: Vec<String> = entry_script_urls(&html, &home, &asset_base)
                .iter()
                .map(|url| url.as_str().to_string())
                .collect();
            let mut unique = urls.clone();
            unique.dedup();
            assert_eq!(unique, expected, "{html}");
        }
    }

    #[test]
    fn signer_names_and_priorities_follow_upstream() {
        let signer = |path: &str| is_signer_url(&abs(path));
        assert!(signer("/x-web/x-web/assets/sign.o-CaaduMJe.js"));
        assert!(signer("/x-web/x-web/assets/sign-x.js?v=1"));
        assert!(signer("/x-web/x-web/assets/sign.json.js"));
        assert!(!signer("/x-web/x-web/assets/design.js"));
        assert!(!signer("/x-web/x-web/assets/signature.js"));
        assert!(!signer("/x-web/sign.o-a.js/other.js"));
        assert!(!signer("/x-web/x-web/assets/sign.o-a.mjs"));

        let priority = |path: &str| module_priority(&abs(path));
        assert_eq!(priority("/x-web/x-web/assets/sign.o-a.js"), 0);
        assert_eq!(priority("/x-web/x-web/assets/sign-a.js"), 0);
        assert_eq!(priority("/x-web/x-web/assets/sentry-filter-a.js"), 1);
        assert_eq!(
            priority("/x-web/x-web/assets/client-transaction-id-plugin-a.js"),
            1
        );
        assert_eq!(priority("/x-web/x-web/assets/design.js"), 2);
        assert_eq!(priority("/x-web/x-web/assets/signature.js"), 2);
    }

    #[test]
    fn entry_scripts_come_only_from_the_asset_origin() {
        let home = Url::parse("https://x.com/").unwrap();
        let asset_base = abs("/");
        let entries = |html: &str| -> Vec<String> {
            entry_script_urls(html, &home, &asset_base)
                .iter()
                .map(|url| url.as_str().to_string())
                .collect()
        };
        let entry = "https://abs.twimg.com/x-web/x-web/entry-client-logged-out-abc.js";
        assert_eq!(
            entries(&format!(
                r#"<script type="module" async src="{entry}" nonce="n"></script>"#
            )),
            [entry]
        );
        assert_eq!(
            entries(&format!("<SCRIPT TYPE='module' SRC='{entry}'></SCRIPT>")),
            [entry]
        );
        assert_eq!(
            entries(&format!(
                "<script\n  type=\"module\"\n  src=\"{entry}\"\n></script>"
            )),
            [entry]
        );
        assert_eq!(
            entries(r#"<script src="https://abs.twimg.com/x-web/x-web/other-abc.js"></script>"#),
            Vec::<String>::new()
        );
        for hostile in [
            "https://evil.example/x-web/entry-client-x.js",
            "http://abs.twimg.com/x-web/x-web/entry-client-x.js",
            "https://abs.twimg.com/other/entry-client-x.js",
            "https://abs.twimg.com/x-web/entry-client-x.json",
            "https://user@abs.twimg.com/x-web/entry-client-x.js",
            "https://abs.twimg.com:8443/x-web/entry-client-x.js",
            "//evil.example/x-web/entry-client-x.js",
        ] {
            assert!(
                entries(&format!(r#"<script src="{hostile}"></script>"#)).is_empty(),
                "{hostile}"
            );
        }
        assert_eq!(
            entries(r#"<script src="/x-web/x-web/entry-client-rel.js"></script>"#),
            Vec::<String>::new(),
            "x.com 起点の相対パスは abs.twimg.com ではないので通さない"
        );
    }

    /// PR #5 のフィクスチャを現行 X の生成器で取った期待値 (時刻・乱数固定) と照合する。
    /// フィクスチャは qtmleap/x-transaction の `src/fixtures/` で、公開されている X のモジュールの抜粋。
    #[test]
    fn matches_the_native_generator_of_the_current_x_web_assets() {
        let html = include_str!("../scripts/fixtures/x-web-home.html");
        let signer = include_str!("../scripts/fixtures/x-web-sign.js");
        let transaction = Transaction::new(html, signer).unwrap();
        let time = (1_700_000_000_000_i64 - TWITTER_EPOCH_MS).div_euclid(1000);
        assert_eq!(
            transaction.generate_with("GET", "/i/api/graphql/test/Test", time, 128),
            "gARMwg5e+JFOvnM1sC+kcsC3oVQaD2Oo7qigITAc/z4cvThEGg5Ei8OAaRmhA/iPbRANhIElp9rZbwoVr/tz+nJbBfEhgw"
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
}
