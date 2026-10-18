//! ホーム HTML と X のモジュールから署名モジュールを探す処理。

use super::compile;
use regex::Regex;
use reqwest::Url;
use std::sync::LazyLock;

pub(super) static ON_DEMAND_INDEX: LazyLock<Regex> =
    LazyLock::new(|| compile(r#",([0-9]+):["']ondemand\.s["']"#));
pub(super) static ON_DEMAND_ENTRY: LazyLock<Regex> =
    LazyLock::new(|| compile(r#",([0-9]+):["']([0-9a-f]+)["']"#));
/// 上流 discovery.ts の `<script src>` 抽出。ASCII の空白と境界だけを使う。
pub(super) static SCRIPT_SRC: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"<(?i-u:script)(?-u:\b)[^>]*(?-u:\b)(?i-u:src)(?-u:\s)*=(?-u:\s)*["']([^"']+)["'][^>]*>"#,
    )
});
/// 上流 discovery.ts の import / dynamic import / re-export 抽出。
pub(super) static MODULE_IMPORT: LazyLock<Regex> = LazyLock::new(|| {
    compile(
        r#"(?-u:\b)(?:import(?-u:\s)*\((?-u:\s)*|import(?-u:\s)*(?:[^"'`;]*?(?-u:\b)from(?-u:\s)*)?|export(?-u:\s)+[^;]*?(?-u:\b)from(?-u:\s)*)["'`]([^"'`]+)["'`]"#,
    )
});
pub(super) static SIGNER_PATH: LazyLock<Regex> =
    LazyLock::new(|| compile(r"/sign[.-][^/]*\.js(?:\?|$)"));

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
