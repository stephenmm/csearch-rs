use csearch::query::Query;
use csearch::read::Index;
use csearch::regexp;
use csearch::trigram;
use csearch::write::{build_index, BuildOptions};
use std::fs;

#[test]
fn build_and_query() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("src");
    fs::create_dir_all(root.join("sub")).unwrap();
    fs::write(root.join("a.txt"), "hello world\nfoo bar baz\n").unwrap();
    fs::write(root.join("b.txt"), "goodbye world\n").unwrap();
    fs::write(
        root.join("sub/c.rs"),
        "fn main() { println!(\"hello\"); }\n",
    )
    .unwrap();
    fs::write(root.join("bin.dat"), b"abc\0def").unwrap(); // skipped: NUL
    fs::write(root.join(".hidden"), "hello hidden").unwrap(); // skipped: dotfile
    fs::write(root.join("tiny"), "hi").unwrap(); // indexed, no trigrams

    let out = dir.path().join("index");
    let stats = build_index(std::slice::from_ref(&root), &out, &BuildOptions::default()).unwrap();
    assert_eq!(stats.files_indexed, 4);
    assert_eq!(stats.files_skipped, 1);

    let idx = Index::open(&out).unwrap();
    assert_eq!(idx.num_files(), 4);
    assert_eq!(idx.roots().len(), 1);
    let names: Vec<&str> = (0..idx.num_files()).map(|i| idx.name(i)).collect();
    assert!(names.iter().any(|n| n.ends_with("a.txt")));
    assert!(names.iter().any(|n| n.ends_with("c.rs")));
    assert!(!names.iter().any(|n| n.ends_with(".hidden")));

    let hel = trigram::pack(b"hel");
    assert_eq!(idx.posting_count(hel), 2);
    let ids = idx.posting_list(hel);
    assert_eq!(ids.len(), 2);
    assert!(idx.name(ids[0]).ends_with("a.txt"));
    assert!(idx.name(ids[1]).ends_with("c.rs"));

    let q = regexp::regexp_query("hello wor", false).unwrap();
    let files: Vec<&str> = idx
        .posting_query(&q)
        .into_iter()
        .map(|i| idx.name(i))
        .collect();
    assert_eq!(files.len(), 1);
    assert!(files[0].ends_with("a.txt"));

    let q = regexp::regexp_query("world", false).unwrap();
    assert_eq!(idx.posting_query(&q).len(), 2);

    let q = regexp::regexp_query("hello|goodbye", false).unwrap();
    assert_eq!(idx.posting_query(&q).len(), 3);

    let q = regexp::regexp_query("nothing_here", false).unwrap();
    assert!(idx.posting_query(&q).is_empty());

    // Unconstrained queries return every file.
    assert_eq!(idx.posting_query(&Query::all()).len(), 4);
}
