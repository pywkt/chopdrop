//! The XDND `text/uri-list` parser patched into the vendored baseview
//! (`vendor/UPSTREAMING.md` §Patch 1).
//!
//! baseview is a dev-dependency here purely for this: `[patch.crates-io]` points
//! it at `vendor/baseview`, and a vendored crate's own `#[cfg(test)]` tests do
//! not run under `cargo test --workspace`.
//!
//! The parser resolves nothing on disk — none of these paths need to exist.

use baseview::parse_uri_list;
use std::path::PathBuf;

fn paths(data: &str) -> Vec<PathBuf> {
    parse_uri_list(data)
}

fn one(data: &str) -> PathBuf {
    let mut list = paths(data);
    assert_eq!(list.len(), 1, "expected exactly one path from {data:?}, got {list:?}");
    list.pop().unwrap()
}

/// Bitwig's own shape: one slash after `file:`, and a percent-encoded space.
#[test]
fn single_slash_file_uri_percent_decoded() {
    assert_eq!(
        one("file:/home/user/Music/BD%20606.wav"),
        PathBuf::from("/home/user/Music/BD 606.wav")
    );
}

#[test]
fn three_slash_file_uri() {
    assert_eq!(one("file:///home/user/kick.wav"), PathBuf::from("/home/user/kick.wav"));
}

#[test]
fn localhost_file_uri() {
    assert_eq!(one("file://localhost/home/user/kick.wav"), PathBuf::from("/home/user/kick.wav"));
    // The hostname is case-insensitive.
    assert_eq!(one("file://LocalHost/tmp/snare.wav"), PathBuf::from("/tmp/snare.wav"));
}

#[test]
fn percent_escapes_other_than_space() {
    assert_eq!(one("file:/tmp/100%25%20wet.wav"), PathBuf::from("/tmp/100% wet.wav"));
    assert_eq!(one("file:/tmp/a%2Bb%23c.wav"), PathBuf::from("/tmp/a+b#c.wav"));
    // Non-ASCII, percent-encoded as UTF-8 bytes.
    assert_eq!(one("file:/tmp/caf%C3%A9.wav"), PathBuf::from("/tmp/café.wav"));
}

/// Sources that do not encode at all still have to work.
#[test]
fn literal_space_is_kept() {
    assert_eq!(one("file:/tmp/BD 606.wav"), PathBuf::from("/tmp/BD 606.wav"));
}

#[test]
fn several_files_crlf() {
    assert_eq!(
        paths("file:/tmp/a.wav\r\nfile:///tmp/b%20two.wav\r\nfile://localhost/tmp/c.wav\r\n"),
        vec![
            PathBuf::from("/tmp/a.wav"),
            PathBuf::from("/tmp/b two.wav"),
            PathBuf::from("/tmp/c.wav"),
        ]
    );
}

#[test]
fn several_files_lf_only() {
    assert_eq!(
        paths("file:/tmp/a.wav\nfile:/tmp/b.wav"),
        vec![PathBuf::from("/tmp/a.wav"), PathBuf::from("/tmp/b.wav")]
    );
}

#[test]
fn order_is_preserved() {
    let list = paths("file:/tmp/3.wav\r\nfile:/tmp/1.wav\r\nfile:/tmp/2.wav");
    assert_eq!(
        list,
        vec![PathBuf::from("/tmp/3.wav"), PathBuf::from("/tmp/1.wav"), PathBuf::from("/tmp/2.wav")]
    );
}

#[test]
fn empty_lines_and_comments_are_ignored() {
    assert_eq!(
        paths("# a comment\r\n\r\nfile:/tmp/a.wav\r\n\r\n#another\nfile:/tmp/b.wav\n"),
        vec![PathBuf::from("/tmp/a.wav"), PathBuf::from("/tmp/b.wav")]
    );
}

/// The point of the patch: one bad line must not cost the drop the good ones.
#[test]
fn bad_lines_are_skipped_not_fatal() {
    assert_eq!(
        paths(
            "https://example.com/x.wav\r\n\
             file:/tmp/good.wav\r\n\
             not a uri at all\r\n\
             file:relative/path.wav\r\n\
             file:/tmp/also%20good.wav\r\n\
             file://nas/share/remote.wav\r\n"
        ),
        vec![PathBuf::from("/tmp/good.wav"), PathBuf::from("/tmp/also good.wav")]
    );
}

#[test]
fn non_file_scheme_yields_nothing() {
    assert!(paths("https://example.com/kick.wav").is_empty());
    assert!(paths("sftp://host/kick.wav\r\nmailto:someone@example.com").is_empty());
}

#[test]
fn remote_host_yields_nothing() {
    assert!(paths("file://nas.local/share/kick.wav").is_empty());
    // "localhost" followed by something other than '/' is a different host.
    assert!(paths("file://localhost.example.com/kick.wav").is_empty());
}

#[test]
fn degenerate_input_yields_nothing() {
    assert!(paths("").is_empty());
    assert!(paths("\r\n\r\n").is_empty());
    assert!(paths("file:").is_empty());
    // An authority and no path names no file.
    assert!(paths("file://localhost").is_empty());
}

/// Decoding happens per line, after the split, so an encoded newline cannot
/// forge a second entry.
#[test]
fn encoded_newline_does_not_split_a_line() {
    assert_eq!(one("file:/tmp/weird%0Aname.wav"), PathBuf::from("/tmp/weird\nname.wav"));
}
