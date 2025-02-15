use crate::builders::{CrateBuilder, PublishBuilder, VersionBuilder};
use crate::util::{RequestHelper, TestApp};
use diesel::prelude::*;
use http::StatusCode;
use insta::{assert_display_snapshot, assert_json_snapshot};

#[test]
fn show() {
    let (app, anon, user) = TestApp::init().with_user();
    let user = user.as_model();

    app.db(|conn| {
        use crates_io::schema::versions;
        use diesel::{update, ExpressionMethods};

        let krate = CrateBuilder::new("foo_show", user.id)
            .description("description")
            .documentation("https://example.com")
            .homepage("http://example.com")
            .version(VersionBuilder::new("1.0.0"))
            .version(VersionBuilder::new("0.5.0"))
            .version(VersionBuilder::new("0.5.1"))
            .keyword("kw1")
            .downloads(20)
            .recent_downloads(10)
            .expect_build(conn);

        // Make version 1.0.0 mimic a version published before we started recording who published
        // versions
        let none: Option<i32> = None;
        update(versions::table)
            .filter(versions::num.eq("1.0.0"))
            .set(versions::published_by.eq(none))
            .execute(conn)
            .unwrap();

        krate
    });

    let response = anon.get::<()>("/api/v1/crates/foo_show");
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
        ".keywords[].created_at" => "[datetime]",
        ".versions[].created_at" => "[datetime]",
        ".versions[].updated_at" => "[datetime]",
    });
}

#[test]
fn show_minimal() {
    let (app, anon, user) = TestApp::init().with_user();
    let user = user.as_model();

    app.db(|conn| {
        CrateBuilder::new("foo_show_minimal", user.id)
            .description("description")
            .documentation("https://example.com")
            .homepage("http://example.com")
            .version(VersionBuilder::new("1.0.0"))
            .version(VersionBuilder::new("0.5.0"))
            .version(VersionBuilder::new("0.5.1"))
            .keyword("kw1")
            .downloads(20)
            .recent_downloads(10)
            .expect_build(conn)
    });

    let response = anon.get::<()>("/api/v1/crates/foo_show_minimal?include=");
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });
}

#[test]
fn test_missing() {
    let (_, anon) = TestApp::init().empty();

    let response = anon.get::<()>("/api/v1/crates/missing");
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert_display_snapshot!(response.text(), @r###"{"errors":[{"detail":"crate `missing` does not exist"}]}"###);
}

#[test]
fn version_size() {
    let (_, _, user) = TestApp::full().with_user();

    let crate_to_publish = PublishBuilder::new("foo_version_size", "1.0.0");
    user.publish_crate(crate_to_publish).good();

    // Add a file to version 2 so that it's a different size than version 1
    let crate_to_publish = PublishBuilder::new("foo_version_size", "2.0.0")
        .add_file("foo_version_size-2.0.0/big", "a");
    user.publish_crate(crate_to_publish).good();

    let crate_json = user.show_crate("foo_version_size");

    let version1 = crate_json
        .versions
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.num == "1.0.0")
        .expect("Could not find v1.0.0");
    assert_eq!(version1.crate_size, Some(158));

    let version2 = crate_json
        .versions
        .as_ref()
        .unwrap()
        .iter()
        .find(|v| v.num == "2.0.0")
        .expect("Could not find v2.0.0");
    assert_eq!(version2.crate_size, Some(184));
}

#[test]
fn block_bad_documentation_url() {
    let (app, anon, user) = TestApp::init().with_user();
    let user = user.as_model();

    app.db(|conn| {
        CrateBuilder::new("foo_bad_doc_url", user.id)
            .documentation("http://rust-ci.org/foo/foo_bad_doc_url/doc/foo_bad_doc_url/")
            .expect_build(conn)
    });

    let json = anon.show_crate("foo_bad_doc_url");
    assert_eq!(json.krate.documentation, None);
}

#[test]
fn test_new_name() {
    let (app, anon, user) = TestApp::init().with_user();
    app.db(|conn| CrateBuilder::new("new", user.as_model().id).expect_build(conn));

    let response = anon.get::<()>("/api/v1/crates/new?include=");
    assert_eq!(response.status(), StatusCode::OK);
    assert_json_snapshot!(response.json(), {
        ".crate.created_at" => "[datetime]",
        ".crate.updated_at" => "[datetime]",
    });
}
