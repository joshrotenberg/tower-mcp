//! Tests for query-parameter expansion in URI templates.

use super::*;

fn template(pattern: &str) -> ResourceTemplate {
    ResourceTemplateBuilder::new(pattern).name("t").handler(
        |uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult {
                contents: vec![ResourceContent {
                    uri,
                    mime_type: None,
                    text: Some("body".into()),
                    blob: None,
                    meta: None,
                }],
                meta: None,
                ..Default::default()
            })
        },
    )
}

/// #1282: a template could not carry the metadata a plain resource can,
/// and `definition()` emitted `arguments: []` unconditionally.
///
/// The clone is the point of the assertion. `ResourceTemplate` implements
/// `Clone` by hand because of the `Arc<dyn ..>` handler, so a new field
/// that is not added there is dropped by every router that stores one.
#[test]
fn arguments_and_meta_reach_the_definition_and_survive_a_clone() {
    let template = ResourceTemplateBuilder::new("db://{table}{?limit}")
        .name("rows")
        .argument("table", Some("Table to read"), true)
        .argument("limit", None::<String>, false)
        .handler(|uri: String, _vars: HashMap<String, String>| async move {
            Ok(ReadResourceResult::text(uri, "[]"))
        })
        .with_meta(serde_json::json!({ "example.com/tier": "internal" }))
        .expect("a valid _meta key");

    for (label, definition) in [
        ("original", template.definition()),
        ("clone", template.clone().definition()),
    ] {
        assert_eq!(definition.arguments.len(), 2, "{label} lost its arguments");
        assert_eq!(definition.arguments[0].name, "table");
        assert_eq!(
            definition.arguments[0].description.as_deref(),
            Some("Table to read")
        );
        assert!(definition.arguments[0].required);
        assert!(!definition.arguments[1].required);
        assert!(definition.arguments[1].description.is_none());
        assert_eq!(
            definition
                .meta
                .as_ref()
                .and_then(|m| m.get("example.com/tier")),
            Some(&serde_json::json!("internal")),
            "{label} lost its _meta"
        );
    }
}

/// A template that declares nothing still advertises nothing, so the wire
/// output is unchanged for every server that has not opted in.
#[test]
fn a_template_without_declared_arguments_advertises_none() {
    let definition = template("db://users/{id}").definition();
    assert!(definition.arguments.is_empty());
    assert!(definition.meta.is_none());
}

/// #1253: `{?cursor,limit}` was read as one variable literally named
/// "?cursor,limit", with a required capture, so the base URI did not
/// match at all.
#[test]
fn the_base_uri_matches_when_no_query_variable_is_present() {
    let t = template("agent://threads{?cursor,limit}");
    let vars = t.match_uri("agent://threads").expect("base URI must match");
    assert!(
        vars.is_empty(),
        "no query variables were supplied: {vars:?}"
    );
}

#[test]
fn each_query_variable_arrives_under_its_own_name() {
    let t = template("agent://threads{?cursor,limit}");
    let vars = t
        .match_uri("agent://threads?cursor=abc&limit=20")
        .expect("must match");
    assert_eq!(vars.get("cursor").map(String::as_str), Some("abc"));
    assert_eq!(vars.get("limit").map(String::as_str), Some("20"));
}

/// Every declared variable is optional and order is not significant,
/// which is the routing policy RFC 6570 leaves open.
#[test]
fn any_subset_in_any_order_matches() {
    let t = template("codex://threads{?cursor,limit,cwd}");
    let vars = t
        .match_uri("codex://threads?cwd=%2Ftmp&cursor=x")
        .expect("must match");
    assert_eq!(vars.get("cursor").map(String::as_str), Some("x"));
    assert_eq!(vars.get("cwd").map(String::as_str), Some("/tmp"));
    assert!(!vars.contains_key("limit"), "absent stays absent: {vars:?}");
}

#[test]
fn query_expansion_composes_with_path_variables() {
    let t = template("claude://projects/{project_key}/sessions{?cursor}");
    let vars = t
        .match_uri("claude://projects/abc/sessions?cursor=n2")
        .expect("must match");
    assert_eq!(vars.get("project_key").map(String::as_str), Some("abc"));
    assert_eq!(vars.get("cursor").map(String::as_str), Some("n2"));

    let bare = t
        .match_uri("claude://projects/abc/sessions")
        .expect("must match without a query");
    assert_eq!(bare.get("project_key").map(String::as_str), Some("abc"));
    assert!(!bare.contains_key("cursor"));
}

/// Present-but-empty is a different fact from absent, and a handler that
/// distinguishes them should be able to.
#[test]
fn an_empty_value_is_present_not_absent() {
    let t = template("agent://threads{?cursor}");
    let vars = t.match_uri("agent://threads?cursor=").expect("must match");
    assert_eq!(vars.get("cursor").map(String::as_str), Some(""));
}

/// Routing must not break because a caller appended something we never
/// declared, and a repeated key resolves predictably.
#[test]
fn undeclared_keys_are_ignored_and_the_first_duplicate_wins() {
    let t = template("agent://threads{?cursor}");
    let vars = t
        .match_uri("agent://threads?utm=x&cursor=first&cursor=second")
        .expect("must match");
    assert_eq!(vars.get("cursor").map(String::as_str), Some("first"));
    assert!(!vars.contains_key("utm"));
}

#[test]
fn values_are_percent_decoded_and_plus_is_a_space() {
    let t = template("agent://search{?q}");
    let vars = t
        .match_uri("agent://search?q=a%20b+c%2Fd")
        .expect("must match");
    assert_eq!(vars.get("q").map(String::as_str), Some("a b c/d"));
}

/// A percent sign next to a multibyte character used to panic: the
/// decoder sliced the string by byte offset after checking only length,
/// so either end of the slice could land inside a character. Any client
/// could reach it through a query expression, because even an undeclared
/// key is decoded before it is discarded.
#[test]
fn percent_decoding_survives_multibyte_input() {
    let t = template("agent://search{?q}");
    for (uri, expected) in [
        ("agent://search?q=%a\u{e9}", "%a\u{e9}"),
        ("agent://search?q=a%9\u{e9}", "a%9\u{e9}"),
        ("agent://search?q=\u{e9}%\u{e9}", "\u{e9}%\u{e9}"),
        ("agent://search?q=%\u{1f600}", "%\u{1f600}"),
    ] {
        let vars = t
            .match_uri(uri)
            .unwrap_or_else(|| panic!("must match: {uri}"));
        assert_eq!(
            vars.get("q").map(String::as_str),
            Some(expected),
            "for {uri}"
        );
    }
}

/// An undeclared key is decoded before it is discarded, so it is just as
/// reachable as a declared one.
#[test]
fn an_undeclared_key_with_multibyte_input_is_also_safe() {
    let t = template("agent://search{?q}");
    let vars = t
        .match_uri("agent://search?%a\u{e9}=x&q=fine")
        .expect("must match");
    assert_eq!(vars.get("q").map(String::as_str), Some("fine"));
}

/// A percent at or near the end has no two digits to read.
#[test]
fn a_truncated_escape_is_left_alone() {
    let t = template("agent://search{?q}");
    for (uri, expected) in [
        ("agent://search?q=%", "%"),
        ("agent://search?q=%2", "%2"),
        ("agent://search?q=a%", "a%"),
        ("agent://search?q=%zz", "%zz"),
    ] {
        let vars = t.match_uri(uri).expect("must match");
        assert_eq!(
            vars.get("q").map(String::as_str),
            Some(expected),
            "for {uri}"
        );
    }
}

/// A malformed escape reaches the handler as written rather than
/// disappearing, so the caller can see what they sent.
#[test]
fn a_malformed_escape_is_left_alone() {
    let t = template("agent://search{?q}");
    let vars = t.match_uri("agent://search?q=100%").expect("must match");
    assert_eq!(vars.get("q").map(String::as_str), Some("100%"));
}

/// The `&` continuation form is the same expansion.
#[test]
fn the_continuation_operator_is_supported() {
    let t = template("agent://threads{&cursor}");
    let vars = t.match_uri("agent://threads?cursor=z").expect("must match");
    assert_eq!(vars.get("cursor").map(String::as_str), Some("z"));
}

/// A template without a query expression keeps matching a literal `?`
/// exactly as before, which is what stops this being a breaking change.
#[test]
fn templates_without_a_query_expression_are_unchanged() {
    let compiled = compile_uri_template("http://example.com/api?query={q}").unwrap();
    assert!(compiled.query_variables.is_empty());
    assert_eq!(compiled.variables, vec!["q".to_string()]);

    let t = template("http://example.com/api?query={q}");
    let vars = t
        .match_uri("http://example.com/api?query=hello")
        .expect("must match");
    assert_eq!(vars.get("q").map(String::as_str), Some("hello"));
}

#[test]
fn a_query_expression_must_end_the_template() {
    assert!(compile_uri_template("agent://x{?a}/more").is_err());
    assert!(compile_uri_template("agent://x{?a}{?b}").is_err());
    assert!(compile_uri_template("agent://x{?}").is_err());
    assert!(compile_uri_template("agent://x{?a,}").is_err());
}
