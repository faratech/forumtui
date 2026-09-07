//! Print the quote block the client would build for a post, so the site's
//! own ContentIntegrity analyzer can be run against it (#707).
fn main() {
    let a: Vec<String> = std::env::args().collect();
    let msg = std::fs::read_to_string(&a[1]).expect("message file");
    print!(
        "{}",
        common::bbcode::quote_block(&a[2], a[3].parse().unwrap(), a[4].parse().unwrap(), &msg)
    );
}
