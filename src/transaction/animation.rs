//! ondemand.s とホーム HTML から animation key を作る処理。

use super::math::{Cubic, custom_round, float_to_hex, interpolate, is_odd, js_round};
use super::{TransactionError, WS, compile};
use regex::Regex;
use std::sync::LazyLock;

const TOTAL_ANIMATION_TIME: f64 = 4096.0;
/// アニメーション行に必要な数値の数 (色 6 + 回転 1 + ベジェ 4)。
const FRAME_VALUES: usize = 11;

pub(super) static KEY_INDEX: LazyLock<Regex> =
    LazyLock::new(|| compile(&format!(r"\([A-Za-z0-9_]\[([0-9][0-9]?)\],{WS}*16\)")));
pub(super) static SVG_ANIMATION: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"<(?i-u:svg)[^>]*(?i-u:id)=["'](?i-u:loading-x-anim-)[0-9]+["'][^>]*>(?s:.)*?</(?i-u:svg)>"#,
    )
});
pub(super) static PATH_D: LazyLock<Regex> =
    LazyLock::new(|| compile(r#"<(?i-u:path)[^>]*(?i-u:d)=["']([^"']+)["']"#));

/// ondemand.s から (行を選ぶ添字, 残りのキーバイト添字) を取り出す。
pub(super) fn parse_key_byte_indices(
    ondemand_js: &str,
) -> Result<(usize, Vec<usize>), TransactionError> {
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
pub(super) fn skip_utf16_units(text: &str, units: usize) -> &str {
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
pub(super) fn forgiving_base64_decode(input: &str) -> Option<Vec<u8>> {
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

pub(super) fn animation_key(
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
