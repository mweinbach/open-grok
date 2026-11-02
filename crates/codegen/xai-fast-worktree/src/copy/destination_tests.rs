use super::Destination;
use std::fs;
use std::path::Path;

#[test]
fn replacement_detaches_existing_hard_links() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("dest");
    let source = tmp.path().join("source");
    let sentinel = tmp.path().join("sentinel");
    fs::create_dir(&root).unwrap();
    fs::write(&source, "new").unwrap();
    fs::write(&sentinel, "keep").unwrap();
    fs::hard_link(&sentinel, root.join("old")).unwrap();
    let mut dest = Destination::open(&root).unwrap();
    dest.copy_file(&source, Path::new("old")).unwrap();
    assert_eq!(fs::read(root.join("old")).unwrap(), b"new");
    assert_eq!(fs::read(&sentinel).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn existing_directory_and_leaf_links_cannot_redirect_writes() {
    use std::os::unix::fs::symlink;

    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::create_dir(&root).unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, "new").unwrap();
    fs::write(outside.join("sentinel"), "keep").unwrap();
    symlink(&outside, root.join("cache")).unwrap();
    symlink(outside.join("sentinel"), root.join("leaf")).unwrap();
    let mut dest = Destination::open(&root).unwrap();
    assert!(dest.create_dir(Path::new("cache/dir")).is_err());
    assert!(dest.copy_file(&source, Path::new("cache/new")).is_err());
    assert!(
        dest.symlink(Path::new("target"), Path::new("cache/link"))
            .is_err()
    );
    assert!(dest.copy_file(&source, Path::new("leaf")).is_err());
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(fs::read(outside.join("sentinel")).unwrap(), b"keep");
}

#[cfg(unix)]
#[test]
fn replacing_a_cached_parent_cannot_redirect_subsequent_writes() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    let source = tmp.path().join("source");
    fs::write(&source, "bytes").unwrap();
    let mut dest = Destination::open(&root).unwrap();
    dest.create_dir(Path::new("cache")).unwrap();
    fs::rename(root.join("cache"), root.join("original")).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("cache")).unwrap();
    dest.copy_file(&source, Path::new("cache/new")).unwrap();
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert_eq!(fs::read(root.join("original/new")).unwrap(), b"bytes");
}

#[cfg(unix)]
#[test]
fn ignored_only_copy_reports_a_destination_directory_link() {
    xai_test_utils::require_git!();
    let tmp = tempfile::tempdir().unwrap();
    let source = tmp.path().join("source");
    let dest = tmp.path().join("dest");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(source.join("cache")).unwrap();
    fs::create_dir(&dest).unwrap();
    fs::create_dir(&outside).unwrap();
    xai_test_utils::git::init_git_repo(&source);
    fs::write(source.join(".gitignore"), "cache/\n").unwrap();
    fs::write(source.join("cache/payload"), "new").unwrap();
    fs::write(outside.join("payload"), "keep").unwrap();
    std::os::unix::fs::symlink(&outside, dest.join("cache")).unwrap();

    let err = crate::WorktreeBuilder::new(&source, &dest)
        .parallelism(2)
        .copy_ignored_only()
        .unwrap_err();
    assert!(format!("{err:#}").contains("destination link"), "{err:#}");
    assert_eq!(fs::read(outside.join("payload")).unwrap(), b"keep");
}
