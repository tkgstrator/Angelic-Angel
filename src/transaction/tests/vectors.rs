use crate::transaction::{TWITTER_EPOCH_MS, Transaction, ondemand_file_hash};
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
        serde_json::from_str(include_str!("../../../scripts/x-transaction-vectors.json")).unwrap();
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

/// PR #5 のフィクスチャを現行 X の生成器で取った期待値 (時刻・乱数固定) と照合する。
/// フィクスチャは qtmleap/x-transaction の `src/fixtures/` で、公開されている X のモジュールの抜粋。
#[test]
fn matches_the_native_generator_of_the_current_x_web_assets() {
    let html = include_str!("../../../scripts/fixtures/x-web-home.html");
    let signer = include_str!("../../../scripts/fixtures/x-web-sign.js");
    let transaction = Transaction::new(html, signer).unwrap();
    let time = (1_700_000_000_000_i64 - TWITTER_EPOCH_MS).div_euclid(1000);
    assert_eq!(
        transaction.generate_with("GET", "/i/api/graphql/test/Test", time, 128),
        "gARMwg5e+JFOvnM1sC+kcsC3oVQaD2Oo7qigITAc/z4cvThEGg5Ei8OAaRmhA/iPbRANhIElp9rZbwoVr/tz+nJbBfEhgw"
    );
}
