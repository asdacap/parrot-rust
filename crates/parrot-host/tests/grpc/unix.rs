#![cfg(any(target_os = "linux", target_os = "macos"))]

use parrot_host::grpc::unix::{self, UnixSocketIdentity};
use std::{
    fs, io,
    os::unix::{
        fs::{PermissionsExt, symlink},
        net::{UnixListener, UnixStream},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

#[test]
fn unix_transport_is_owner_only_and_reachable() {
    let directory = TestDirectory::new();
    let path = directory.0.join("control/parrot.sock");
    let (listener, owned) = unix::bind(&path).unwrap();
    assert_eq!(
        fs::metadata(path.parent().unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(owned.path(), path);
    assert_eq!(
        UnixSocketIdentity::read(&path).unwrap(),
        Some(owned.identity())
    );
    let connection = UnixStream::connect(&path).unwrap();
    let _accepted = listener.accept().unwrap();
    drop(connection);
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        io::ErrorKind::WouldBlock
    );
    drop(listener);
    assert!(owned.cleanup().unwrap());
    assert!(!owned.cleanup().unwrap());
    assert!(!path.exists());
}

#[test]
fn active_unix_transport_is_not_replaced() {
    let directory = TestDirectory::new();
    let path = directory.0.join("control/parrot.sock");
    let (listener, owned) = unix::bind(&path).unwrap();
    let error = unix::bind(&path).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
    assert!(error.to_string().contains("active"));
    assert_eq!(
        UnixSocketIdentity::read(&path).unwrap(),
        Some(owned.identity())
    );
    let _first_probe = listener.accept().unwrap();
    let _client = UnixStream::connect(&path).unwrap();
    let _still_reachable = listener.accept().unwrap();
}

#[test]
fn stale_unix_transport_is_removed_explicitly() {
    let directory = TestDirectory::new();
    let path = directory.0.join("stale.sock");
    let stale = UnixListener::bind(&path).unwrap();
    assert!(UnixSocketIdentity::read(&path).unwrap().is_some());
    drop(stale);
    let (listener, owned) = unix::bind(&path).unwrap();
    assert!(UnixStream::connect(&path).is_ok());
    assert!(listener.accept().is_ok());
    assert_eq!(
        UnixSocketIdentity::read(&path).unwrap(),
        Some(owned.identity())
    );
}

#[test]
fn unix_transport_refuses_regular_and_symbolic_link_entries() {
    let directory = TestDirectory::new();
    for symbolic_link in [false, true] {
        let path = directory.0.join(if symbolic_link {
            "link.sock"
        } else {
            "file.sock"
        });
        let target = directory.0.join("preserved");
        fs::write(&target, "preserved").unwrap();
        if symbolic_link {
            symlink(&target, &path).unwrap();
        } else {
            fs::write(&path, "preserved").unwrap();
        }
        assert_eq!(
            unix::bind(&path).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), "preserved");
        assert_eq!(fs::read_to_string(&target).unwrap(), "preserved");
    }
}

#[test]
fn unix_transport_refuses_parent_symlinks_without_chmodding_the_target() {
    let directory = TestDirectory::new();
    let target = directory.0.join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o755)).unwrap();
    let link = directory.0.join("link");
    symlink(&target, &link).unwrap();
    let path = link.join("missing/control/parrot.sock");
    assert_eq!(
        unix::bind(&path).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        fs::metadata(&target).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert!(!target.join("missing").exists());
}

#[test]
fn long_paths_fail_before_directory_creation_or_permission_changes() {
    let directory = TestDirectory::new();
    let parent = directory.0.join("not-created");
    let path = parent.join(format!("{}.sock", "a".repeat(150)));
    assert!(unix::bind(&path).is_err());
    assert!(!parent.exists());
    assert_eq!(
        unix::bind(Path::new("relative.sock")).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

#[test]
fn cleanup_preserves_a_successor_regular_file_or_symlink() {
    let directory = TestDirectory::new();
    for symbolic_link in [false, true] {
        let path = directory.0.join(if symbolic_link {
            "link.sock"
        } else {
            "file.sock"
        });
        let (listener, owned) = unix::bind(&path).unwrap();
        fs::remove_file(&path).unwrap();
        let target = directory.0.join("successor-target");
        fs::write(&target, "successor preserved").unwrap();
        if symbolic_link {
            symlink(&target, &path).unwrap();
        } else {
            fs::write(&path, "successor preserved").unwrap();
        }
        assert!(!owned.cleanup().unwrap());
        drop(listener);
        drop(owned);
        assert_eq!(fs::read_to_string(&path).unwrap(), "successor preserved");
        assert_eq!(
            fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_symlink(),
            symbolic_link
        );
    }
}

#[test]
fn cleanup_preserves_a_bound_successor_socket_identity() {
    let directory = TestDirectory::new();
    let path = directory.0.join("socket.sock");
    let (original, owned) = unix::bind(&path).unwrap();
    fs::remove_file(&path).unwrap();
    let successor = UnixListener::bind(&path).unwrap();
    let successor_identity = UnixSocketIdentity::read(&path).unwrap().unwrap();
    assert_ne!(successor_identity, owned.identity());
    assert!(!owned.cleanup().unwrap());
    drop(original);
    drop(owned);
    assert_eq!(
        UnixSocketIdentity::read(&path).unwrap(),
        Some(successor_identity)
    );
    let _client = UnixStream::connect(&path).unwrap();
    let _accepted = successor.accept().unwrap();
}

#[test]
fn cleanup_after_concurrent_successor_publication_preserves_successor() {
    let directory = TestDirectory::new();
    let path = directory.0.join("socket.sock");
    let (original, owned) = unix::bind(&path).unwrap();
    let replacement_path = path.clone();
    let (published, ready) = std::sync::mpsc::channel();
    let replacement = std::thread::spawn(move || {
        fs::remove_file(&replacement_path).unwrap();
        let listener = UnixListener::bind(&replacement_path).unwrap();
        published
            .send(
                UnixSocketIdentity::read(&replacement_path)
                    .unwrap()
                    .unwrap(),
            )
            .unwrap();
        let _accepted = listener.accept().unwrap();
    });
    let successor = ready.recv().unwrap();
    assert_ne!(successor, owned.identity());
    assert!(!owned.cleanup().unwrap());
    drop(owned);
    drop(original);
    assert_eq!(UnixSocketIdentity::read(&path).unwrap(), Some(successor));
    let _client = UnixStream::connect(&path).unwrap();
    replacement.join().unwrap();
}

#[test]
fn identity_reads_missing_and_non_directory_components_as_absent() {
    let directory = TestDirectory::new();
    assert_eq!(
        UnixSocketIdentity::read(&directory.0.join("absent.sock")).unwrap(),
        None
    );
    let regular = directory.0.join("file");
    fs::write(&regular, "preserved").unwrap();
    assert_eq!(
        UnixSocketIdentity::read(&regular.join("socket")).unwrap(),
        None
    );
    assert_eq!(
        UnixSocketIdentity::read(&regular).unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
}

struct TestDirectory(PathBuf);
impl TestDirectory {
    fn new() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = fs::canonicalize("/tmp").unwrap().join(format!(
            "parrot-t63-unix-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }
}
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
