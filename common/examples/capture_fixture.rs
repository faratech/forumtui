//! Capture an API response into `common/src/testdata/` as a test fixture.
//!
//! Issue #713. Three bugs shipped behind fixtures that disagreed with the
//! wire — `rating_average` for `rating_avg` (#696), a map for `tags` when
//! the wire sends an array (#698), and both halves of the attachment upload
//! (#709) — because the fixture was hand-written, or re-captured through a
//! `jq` filter that silently dropped `tags` entirely.
//!
//! So this tool has exactly one rule: it never selects fields. It parses the
//! body only to pretty-print it (which preserves every key), and refuses a
//! body that is not JSON rather than saving something a test will misread.
//!
//! ```text
//! WFTUI_API_KEY=... cargo run -p common --example capture_fixture -- \
//!     https://windowsforum.com /media/?page=1 media_page.json
//! ```
//!
//! Read-only: it issues a single GET and writes one file under `testdata/`.
//! Redact personal values by hand afterwards if the body carries a member
//! profile — keep every key, change only the value.
#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().collect();
    let [_, origin, path, name] = args.as_slice() else {
        eprintln!("usage: capture_fixture <origin> <api-path> <name.json>");
        std::process::exit(2);
    };

    // The name is a bare file name, so a captured fixture can only ever land
    // in `testdata/` — no `../` walking out of the crate.
    if name.contains('/') || name.contains('\\') || !name.ends_with(".json") {
        eprintln!("name must be a bare `<something>.json`, got {name:?}");
        std::process::exit(2);
    }
    let Ok(key) = std::env::var("WFTUI_API_KEY") else {
        eprintln!("set WFTUI_API_KEY (see /web/.env)");
        std::process::exit(2);
    };

    let url = format!("{}/api{}", origin.trim_end_matches('/'), path);
    // `http::build` rather than `reqwest::Client::new`: the crate takes
    // reqwest's `rustls-no-provider` feature and installs ring itself, so a
    // bare client has no crypto provider and every https request fails.
    let resp = common::http::build()
        .expect("http client")
        .get(&url)
        .header("XF-Api-Key", key)
        .send()
        .await
        .expect("request failed");
    let status = resp.status();
    let body = resp.text().await.expect("read body");

    // A non-JSON body is Cloudflare or an XF error page. Saving it would
    // produce a fixture that deserializes to nothing useful.
    let parsed: serde_json::Value = match serde_json::from_str(&body) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{url} -> {status}: body is not JSON ({e})");
            eprintln!("{}", &body[..body.len().min(400)]);
            std::process::exit(1);
        }
    };
    if !status.is_success() {
        eprintln!("{url} -> {status}");
        eprintln!("{}", serde_json::to_string_pretty(&parsed).unwrap_or(body));
        std::process::exit(1);
    }

    let out = concat!(env!("CARGO_MANIFEST_DIR"), "/src/testdata/").to_string() + name;
    let mut text = serde_json::to_string_pretty(&parsed).expect("re-serialize");
    text.push('\n');
    std::fs::write(&out, text).expect("write fixture");
    println!("{url} -> {status}, wrote {out}");
}
