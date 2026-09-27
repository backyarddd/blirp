//! The file engine's copy algorithms against the hub service in-process:
//! a model-based property test with several writers, and what a receiver
//! does with paths it must not or cannot write.

use super::copy::{self, Copy, Env};
use blirp_core::files::scan::ScanConfig;
use blirp_core::files::{ChangeOp, EntryContent, FileChange, hash_bytes};
use blirp_core::store::{FileCopy, Store};
use blirp_sync::files::blobs::BlobStore;
use blirp_sync::files::{FileHub, HubFiles, LocalHub};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    hub: Arc<HubFiles>,
    writers: Vec<(Env, Copy)>,
}

const FILES: &[&str] = &["a.txt", "d/b.txt", "c.txt"];

fn scan_cfg(dir: &Path) -> ScanConfig {
    ScanConfig {
        max_file_bytes: 1 << 20,
        max_root_bytes: 1 << 30,
        max_files: 1000,
        data_dir: Some(dir.join("data")),
    }
}

fn env(w: &World, i: usize) -> Env {
    Env {
        store: w.store.clone(),
        hub: FileHub::Local(Arc::new(LocalHub {
            hub: w.hub.clone(),
            machine_id: format!("m{i}"),
            machine_name: format!("w{i}"),
            parts: BlobStore::new(w.dir.path().join(format!("dl{i}"))),
        })),
        data_dir: w.dir.path().join("data"),
        scan: scan_cfg(w.dir.path()),
        gate: Arc::new(tokio::sync::Semaphore::new(1)),
    }
}

/// An origin (writer 0) and `copies` downloaded copies.
async fn world(copies: usize) -> World {
    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(&dir.path().join("db")).unwrap());
    let hub = Arc::new(HubFiles::new(
        store.clone(),
        &dir.path().join("hubfiles"),
        1 << 30,
        1 << 40,
    ));
    let origin = dir.path().join("w0");
    std::fs::create_dir_all(&origin).unwrap();
    // A root exists on the hub once its origin uploaded something.
    std::fs::write(origin.join("seed.txt"), "seed").unwrap();
    let p = store.register_project("m0", &origin, Some("p")).unwrap();
    let key = store.project_paths(&p.id).unwrap()[0].path.clone();
    let root_id = blirp_core::files::root_id("m0", &key);
    let mut w = World {
        dir,
        store,
        hub,
        writers: Vec::new(),
    };
    let e0 = env(&w, 0);
    let c0 = Copy {
        key,
        root_id: root_id.clone(),
        origin: true,
    };
    copy::upload(&e0, &c0).await.unwrap();
    w.writers.push((e0, c0));
    for i in 1..=copies {
        let folder = w.dir.path().join(format!("w{i}"));
        std::fs::create_dir_all(&folder).unwrap();
        let key = dunce::canonicalize(&folder).unwrap().display().to_string();
        w.store
            .put_file_copy(&FileCopy {
                path: key.clone(),
                root_id: root_id.clone(),
                origin: false,
                seen: 0,
                created_at: 0,
                detached: false,
            })
            .unwrap();
        let e = env(&w, i);
        let c = Copy {
            key,
            root_id: root_id.clone(),
            origin: false,
        };
        copy::apply(&e, &c, true).await.unwrap();
        w.writers.push((e, c));
    }
    w
}

/// Every file under `root` (wire path -> content), temp files excluded.
fn files_of(root: &Path) -> BTreeMap<String, String> {
    fn walk(dir: &Path, rel: &str, out: &mut BTreeMap<String, String>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let wire = format!("{rel}{name}");
            if e.file_type().unwrap().is_dir() {
                walk(&e.path(), &format!("{wire}/"), out);
            } else {
                out.insert(wire, std::fs::read_to_string(e.path()).unwrap());
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, "", &mut out);
    out
}

#[derive(Debug, Clone)]
enum Op {
    Write {
        w: usize,
        f: usize,
    },
    Delete {
        w: usize,
        f: usize,
    },
    Upload {
        w: usize,
    },
    Apply {
        w: usize,
    },
    /// A crash lost the bases a writer had saved.
    LoseBases {
        w: usize,
    },
}

fn op(writers: usize) -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0..writers, 0..FILES.len()).prop_map(|(w, f)| Op::Write { w, f }),
        1 => (0..writers, 0..FILES.len()).prop_map(|(w, f)| Op::Delete { w, f }),
        3 => (0..writers).prop_map(|w| Op::Upload { w }),
        2 => (0..writers).prop_map(|w| Op::Apply { w }),
        1 => (0..writers).prop_map(|w| Op::LoseBases { w }),
    ]
}

/// Contents the hub keeps: current entries and history.
fn hub_contents(store: &Store, w: &World) -> HashSet<String> {
    let root = &w.writers[0].1.root_id;
    let (entries, _) = store.hub_file_index(root, 0, 100_000).unwrap().unwrap();
    let mut out: HashSet<String> = entries
        .into_iter()
        .filter_map(|e| match e.content {
            Some(EntryContent::Blob { hash }) => Some(hash),
            _ => None,
        })
        .collect();
    out.extend(store.hub_history_hashes(root).unwrap());
    out
}

async fn run(ops: Vec<Op>) {
    let w = world(2).await;
    let mut n = 0usize;
    // Every content a writer's folder held when an upload looked at it.
    let mut seen: HashSet<String> = HashSet::new();
    for op in ops {
        match op {
            Op::Write { w: i, f } => {
                n += 1;
                let p = PathBuf::from(&w.writers[i].1.key).join(FILES[f]);
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(&p, format!("w{i} write {n}")).unwrap();
            }
            Op::Delete { w: i, f } => {
                let _ = std::fs::remove_file(PathBuf::from(&w.writers[i].1.key).join(FILES[f]));
            }
            Op::Upload { w: i } => {
                let (e, c) = &w.writers[i];
                for t in files_of(Path::new(&c.key)).into_values() {
                    seen.insert(hash_bytes(t.as_bytes()));
                }
                copy::upload(e, c).await.unwrap();
            }
            Op::Apply { w: i } => {
                let (e, c) = &w.writers[i];
                copy::upload(e, c).await.unwrap();
                for t in files_of(Path::new(&c.key)).into_values() {
                    seen.insert(hash_bytes(t.as_bytes()));
                }
                copy::apply(e, c, false).await.unwrap();
            }
            Op::LoseBases { w: i } => {
                let key = &w.writers[i].1.key;
                let lost: Vec<(String, Option<blirp_core::store::Base>)> = w
                    .store
                    .file_bases(key)
                    .unwrap()
                    .into_keys()
                    .map(|p| (p, None))
                    .collect();
                w.store.update_file_bases(key, &lost).unwrap();
            }
        }
    }
    // Quiet: everyone uploads and takes the hub's changes until nothing moves.
    let mut last = Vec::new();
    for _ in 0..6 {
        for (e, c) in &w.writers {
            for t in files_of(Path::new(&c.key)).into_values() {
                seen.insert(hash_bytes(t.as_bytes()));
            }
            copy::upload(e, c).await.unwrap();
            let r = copy::apply(e, c, false).await.unwrap();
            assert!(r.failed.is_empty(), "{:?}", r.failed);
        }
        let now: Vec<BTreeMap<String, String>> = w
            .writers
            .iter()
            .map(|(_, c)| files_of(Path::new(&c.key)))
            .collect();
        if now == last {
            break;
        }
        last = now;
    }
    // Converged: every copy holds the same files.
    for other in &last[1..] {
        assert_eq!(&last[0], other, "copies did not converge");
    }
    // Nothing observed was lost: it is on disk somewhere, on the hub or in history.
    let mut kept = hub_contents(&w.store, &w);
    for files in &last {
        kept.extend(files.values().map(|t| hash_bytes(t.as_bytes())));
    }
    for h in &seen {
        assert!(kept.contains(h), "a synced version was lost");
    }
    // Nothing was written outside the copies' folders or into a VCS folder.
    for (_, c) in &w.writers {
        assert!(
            files_of(Path::new(&c.key))
                .keys()
                .all(|p| !p.split('/').any(|s| s == ".git"))
        );
    }
    let top: HashSet<String> = std::fs::read_dir(w.dir.path())
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let allowed = [
        "db", "db-wal", "db-shm", "hubfiles", "w0", "w1", "w2", "dl0", "dl1", "dl2",
    ];
    assert!(top.iter().all(|t| allowed.contains(&t.as_str())), "{top:?}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 24, ..ProptestConfig::default() })]

    #[test]
    fn writers_converge_and_lose_nothing(ops in proptest::collection::vec(op(3), 1..24)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(run(ops));
    }
}

#[tokio::test]
async fn receivers_refuse_unsafe_paths_and_keep_what_they_cannot_hold() {
    let w = world(1).await;
    let (e1, c1) = &w.writers[1];
    let root1 = PathBuf::from(&c1.key);
    // Another machine commits names this one may not hold.
    let put = |path: &str, data: &[u8]| {
        let src = w.dir.path().join("src");
        std::fs::write(&src, data).unwrap();
        w.hub
            .import(&hash_bytes(data), &src, data.len() as u64)
            .unwrap();
        FileChange {
            path: path.into(),
            base_version: 0,
            op: ChangeOp::Put {
                hash: hash_bytes(data),
                size: data.len() as i64,
                mode_x: false,
                mtime: 0,
            },
        }
    };
    let out = w
        .hub
        .commit(
            "linux",
            "linux",
            &c1.root_id,
            None,
            None,
            &[
                put("CON", b"device name"),
                put("Case.txt", b"upper"),
                put("case.txt", b"lower"),
                put("sub/x.txt", b"through a link"),
                put(".git/hooks/post-checkout", b"#!/bin/sh"),
            ],
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        &out.results[4],
        blirp_core::files::ChangeResult::Rejected { .. }
    ));
    #[cfg(unix)]
    {
        let outside = w.dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, root1.join("sub")).unwrap();
    }
    let r = copy::apply(e1, c1, false).await.unwrap();
    assert!(!root1.join(".git").exists());
    #[cfg(unix)]
    {
        assert!(
            !w.dir.path().join("outside/x.txt").exists(),
            "wrote through a symlinked folder"
        );
        assert!(r.failed.iter().any(|(p, _)| p == "sub/x.txt"), "{r:?}");
    }
    if cfg!(windows) {
        assert!(r.skipped.iter().any(|(p, _)| p == "CON"), "{r:?}");
    }
    if blirp_core::files::path::case_insensitive_fs() {
        // One name is written, the other kept as a conflict copy.
        assert_eq!(
            r.skipped
                .iter()
                .filter(|(_, why)| why.contains("case"))
                .count(),
            1,
            "{r:?}"
        );
        assert_eq!(r.conflicts.len(), 1, "{r:?}");
    }
    // What this machine could not hold is never read as a local delete.
    let report = copy::upload(e1, c1).await.unwrap();
    assert_eq!(report.pending, 0);
    let (entries, _) = w
        .store
        .hub_file_index(&c1.root_id, 0, 100)
        .unwrap()
        .unwrap();
    for name in ["CON", "Case.txt", "case.txt"] {
        assert!(
            entries
                .iter()
                .any(|e| e.path == name && e.content.is_some()),
            "{name} was deleted on the hub"
        );
    }
}
