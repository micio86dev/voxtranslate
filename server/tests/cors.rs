//! The CORS preflight must know about every method the router actually serves.
//!
//! This test exists because of a failure mode that is invisible from the server side. A
//! method missing from `CORS_ALLOWED_METHODS` still routes, still reaches its handler and
//! still answers `curl` — it fails only in a browser, and it fails as a bare "CORS error"
//! with no status code and no body. The dashboard cannot tell that apart from the network
//! being down, so it reports whatever its generic failure copy says.
//!
//! `PUT` was missing for three routes (`voip/settings`, a number's `routing`, a number's
//! `hours`) because the method list was written before any of them existed and nothing
//! connected the two. That is the drift this test closes: it reads the router's own source
//! and fails when a method is served but not allowed.
//!
//! Not DB-gated on purpose. A configuration property this cheap to check must not sit
//! behind a database that CI might not have.

use std::collections::BTreeSet;
use std::path::Path;

use voxtranslate_server::CORS_ALLOWED_METHODS;

/// The HTTP methods axum can route. `OPTIONS` is excluded: it is the preflight itself,
/// answered by the CORS layer rather than by a route.
const ROUTABLE: [&str; 5] = ["get", "post", "put", "patch", "delete"];

/// Every method name the router source registers, in lowercase.
///
/// Two shapes carry a method, and both must be read:
///
/// - `get(handler)` — the imported `axum::routing` constructor that opens a route.
/// - `.put(handler)` — a method chained onto the `MethodRouter` that constructor returned.
///   These never appear in a `use` list, which is precisely why reading imports alone
///   would have missed all three `PUT`s.
///
/// Over-detection is harmless here and deliberately not defended against: a `map.get(k)`
/// picked up by mistake only asserts that `GET` is allowed, which it already is. The test
/// is one-directional — it can only fail when something is served and not allowed.
fn methods_served_by(dir: &Path) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        for entry in std::fs::read_dir(&path).expect("the server source is readable") {
            let entry = entry.expect("a readable directory entry");
            let p = entry.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.extension().and_then(|e| e.to_str()) != Some("rs") {
                continue;
            }
            let src = std::fs::read_to_string(&p).expect("a readable source file");
            for m in ROUTABLE {
                // `get(` opening a route, or `.get(` chained onto one. Requiring the
                // parenthesis keeps bare identifiers in `use` lists and comments out.
                if src.contains(&format!("{m}(")) {
                    found.insert(m.to_string());
                }
            }
        }
    }
    found
}

#[test]
fn every_method_the_router_serves_is_allowed_by_cors() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let allowed: BTreeSet<String> = CORS_ALLOWED_METHODS
        .iter()
        .map(|m| m.as_str().to_lowercase())
        .collect();

    let missing: Vec<String> = methods_served_by(&src)
        .into_iter()
        .filter(|m| !allowed.contains(m))
        .collect();

    assert!(
        missing.is_empty(),
        "these methods are routed but not in CORS_ALLOWED_METHODS: {missing:?}. \
         They work from curl and fail in every browser, as an unexplained CORS error \
         rather than as a status code. Add them to the list in src/lib.rs."
    );
}

#[test]
fn put_is_allowed_because_three_voip_routes_depend_on_it() {
    // Named separately from the drift test above so a regression reads as what it is.
    // `PUT …/voip/settings`, `PUT …/voip/numbers/{id}/routing` and `…/hours` are the
    // three, and dropping PUT breaks saving all of them from the dashboard.
    assert!(
        CORS_ALLOWED_METHODS.contains(&axum::http::Method::PUT),
        "PUT was dropped from the CORS allow-list; the VoIP settings page cannot save"
    );
}

#[test]
fn the_preflight_never_advertises_a_method_the_api_does_not_serve() {
    // The allow-list is an advertisement, not a permission: listing a method nothing
    // routes tells a browser to try something that can only 405. Cheap to keep honest.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let served = methods_served_by(&src);
    for m in CORS_ALLOWED_METHODS {
        let name = m.as_str().to_lowercase();
        if name == "options" {
            continue; // the preflight itself, answered by the layer and never routed
        }
        assert!(
            served.contains(&name),
            "CORS advertises {name} but no route serves it"
        );
    }
}
