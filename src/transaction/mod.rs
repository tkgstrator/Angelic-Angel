//! x-client-transaction-id ジェネレータ。qtmleap/x-transaction (MIT, (c) 2026 QuantumLeap) の
//! `src/transaction.ts` の移植で、ライセンス原文は `LICENSE.x-transaction` に置く。

mod animation;
mod discovery;
#[cfg(test)]
pub(crate) mod fixture;
mod math;
#[cfg(test)]
mod tests;

pub use discovery::{
    asset_url, entry_script_urls, is_signer_url, module_import_urls, module_priority,
    ondemand_file_hash,
};

use animation::{animation_key, forgiving_base64_decode, parse_key_byte_indices};
use base64::{Engine, engine::general_purpose::STANDARD_NO_PAD};
use regex::Regex;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;

const ADDITIONAL_RANDOM_NUMBER: u8 = 3;
const DEFAULT_KEYWORD: &str = "obfiowerehiring";
const TWITTER_EPOCH_MS: i64 = 1_682_924_400_000;

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

static VERIFICATION_KEY: LazyLock<Regex> = LazyLock::new(|| {
    compile(&format!(
        r#"<(?i-u:meta){WS}+(?i-u:name)=["'](?i-u:twitter-site-verification)["']{WS}+(?i-u:content)=["']([^"']+)["']"#
    ))
});

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
