use crate::transaction::{entry_script_urls, is_signer_url, module_import_urls, module_priority};
use reqwest::Url;

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
    let cases: Vec<(String, Vec<&str>)> = vec![
        (
            format!(r#"<script type="module" src="{e}"></script>"#),
            vec![e],
        ),
        (
            format!("<SCRIPT TYPE=\"module\" SRC='{e}'></SCRIPT>"),
            vec![e],
        ),
        (format!(r#"<script src  =  "{e}" async></script>"#), vec![e]),
        (format!(r#"<script data-src="{e}"></script>"#), vec![e]),
        (format!(r#"<scripts src="{e}"></scripts>"#), vec![]),
        (format!("<script src={e}></script>"), vec![]),
        (
            r#"<script src="/x-web/x-web/entry-client-rel.js"></script>"#.into(),
            vec![],
        ),
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
            r#"<script src="https://abs.twimg.com/x-web/x-web/a.js?entry-client"></script>"#.into(),
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
