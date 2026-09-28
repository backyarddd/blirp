//! The file engine's copy algorithms against the hub service in-process:
//! a model-based property test with several writers, and targeted cases
//! (unsafe paths, mass deletes, hub copies made again, copies over clones).

use super::copy::{self, Copy, Env};
use blirp_core::files::scan::ScanConfig;
use blirp_core::files::{ChangeOp, EntryContent, FileChange, FilesMode, hash_bytes};
use blirp_core::store::{Base, CopyMode, FileCopy, Store};
use blirp_sync::files::blobs::BlobStore;
use blirp_sync::files::{FileHub, HubFiles, LocalHub};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

struct World {
    dir: tempfile::TempDir,
    store: Arc<Store>,
    hub: Arc<HubFiles>,
    writers: Vec<(Env, Copy)>,
    /// Test hooks: a write to do right after the next scan of a writer.
    pending_write: Arc<Mutex<Option<(PathBuf, String)>>>,
}

const FILES: &[&str] = &["a.txt", "d/b.txt", "c.txt"];
/// Only in writer 2's folder before its copy was made (a clone's file the
/// hub never had).
const CLONE_ONLY: &str = "extra.txt";

fn scan_cfg(dir: &Path) -> ScanConfig {
    ScanConfig {
        max_file_bytes: 1 << 20,
        max_root_bytes: 1 << 30,
        max_files: 1000,
        data_dir: Some(dir.join("data")),
    }
}

fn env(w: &World, i: usize) -> Env {
    let pending = w.pending_write.clone();
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
        work: Arc::default(),
        retry: Arc::new(blirp_core::files::write::RetryBudget::new(
            blirp_core::files::write::RETRY_BUDGET,
        )),
        after_scan: Some(Arc::new(move || {
            let next = pending.lock().unwrap().take();
            if let Some((p, text)) = next {
                std::fs::create_dir_all(p.parent().unwrap()).unwrap();
                std::fs::write(p, text).unwrap();
            }
        })),
    }
}

/// An origin (writer 0) and `copies` copies made as a download does:
/// registered pending, the hub's files written over whatever the folder
/// held (writer 2's folder starts as a stale "clone"), then finished.
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
    std::fs::write(origin.join("a.txt"), "origin a").unwrap();
    let p = store.register_project("m0", &origin, Some("p")).unwrap();
    let key = store.project_paths(&p.id).unwrap()[0].path.clone();
    let root_id = blirp_core::files::root_id("m0", &key);
    let mut w = World {
        dir,
        store,
        hub,
        writers: Vec::new(),
        pending_write: Arc::default(),
    };
    w.store
        .put_file_copy(&FileCopy {
            path: key.clone(),
            root_id: root_id.clone(),
            origin: true,
            seen: 0,
            created_at: 0,
            mode: CopyMode::OnDemand,
            incarnation: String::new(),
        })
        .unwrap();
    let e0 = env(&w, 0);
    let c0 = Copy {
        key,
        root_id: root_id.clone(),
        origin: true,
        incarnation: String::new(),
    };
    copy::upload(&e0, &c0).await.unwrap();
    w.writers.push((e0, c0));
    let incarnation = w.hub.roots().unwrap().0[0].incarnation.clone();
    for i in 1..=copies {
        let folder = w.dir.path().join(format!("w{i}"));
        std::fs::create_dir_all(&folder).unwrap();
        if i == 2 {
            std::fs::write(folder.join("a.txt"), "stale clone a").unwrap();
            std::fs::write(folder.join(CLONE_ONLY), "only in the clone").unwrap();
        }
        let key = dunce::canonicalize(&folder).unwrap().display().to_string();
        w.store
            .put_file_copy(&FileCopy {
                path: key.clone(),
                root_id: root_id.clone(),
                origin: false,
                seen: 0,
                created_at: 0,
                mode: CopyMode::Pending,
                incarnation: incarnation.clone(),
            })
            .unwrap();
        let e = env(&w, i);
        let c = Copy {
            key: key.clone(),
            root_id: root_id.clone(),
            origin: false,
            incarnation: incarnation.clone(),
        };
        let r = copy::apply(&e, &c, true).await.unwrap();
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        w.store.finish_file_copy(&key).unwrap();
        w.writers.push((e, c));
    }
    w
}

impl World {
    /// Writer `i` as the engine would see it now (its row's incarnation),
    /// or None once it detached.
    fn writer(&self, i: usize) -> Option<(Env, Copy)> {
        let (e, c) = &self.writers[i];
        let row = self.store.file_copy(&c.key).unwrap()?;
        (row.mode == CopyMode::OnDemand).then(|| {
            (
                e.clone(),
                Copy {
                    incarnation: row.incarnation,
                    ..c.clone()
                },
            )
        })
    }

    /// What the engine does before each pass: check copies against the
    /// hub's roots.
    fn reconcile(&self) {
        let roots = self.hub.roots().unwrap().0;
        super::engine::check_roots(&self.store, &roots).unwrap();
    }

    fn index(&self) -> Vec<blirp_core::files::IndexEntry> {
        let root = &self.writers[0].1.root_id;
        self.store
            .hub_file_index(root, 0, 100_000)
            .unwrap()
            .map(|s| s.entries)
            .unwrap_or_default()
    }
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
    if root.is_dir() {
        walk(root, "", &mut out);
    }
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
    /// The file changes between an upload's scan and its blob upload.
    WriteDuringUpload {
        w: usize,
        f: usize,
    },
    /// A crash right after a commit, before its bases were saved.
    UploadThenCrash {
        w: usize,
    },
    /// Another OS commits a name differing from `c.txt` only in case.
    CaseTwin,
    /// The origin leaves `c.txt` out (`.blirpignore`): forgotten on the hub.
    Exclude,
    /// Tombstones pass retention on the hub.
    ExpireTombstones,
    /// The hub copy is deleted (and the origin makes it again).
    DeleteRoot,
}

fn op(writers: usize) -> impl Strategy<Value = Op> {
    prop_oneof![
        8 => (0..writers, 0..FILES.len()).prop_map(|(w, f)| Op::Write { w, f }),
        2 => (0..writers, 0..FILES.len()).prop_map(|(w, f)| Op::Delete { w, f }),
        6 => (0..writers).prop_map(|w| Op::Upload { w }),
        4 => (0..writers).prop_map(|w| Op::Apply { w }),
        1 => (0..writers).prop_map(|w| Op::LoseBases { w }),
        2 => (0..writers, 0..FILES.len()).prop_map(|(w, f)| Op::WriteDuringUpload { w, f }),
        1 => (0..writers).prop_map(|w| Op::UploadThenCrash { w }),
        1 => Just(Op::CaseTwin),
        1 => Just(Op::Exclude),
        1 => Just(Op::ExpireTombstones),
        1 => Just(Op::DeleteRoot),
    ]
}

/// Contents the hub keeps: current entries and history.
fn hub_contents(w: &World) -> HashSet<String> {
    let root = &w.writers[0].1.root_id;
    let mut out: HashSet<String> = w
        .index()
        .into_iter()
        .filter_map(|e| match e.content {
            Some(EntryContent::Blob { hash }) => Some(hash),
            _ => None,
        })
        .collect();
    out.extend(w.store.hub_history_hashes(root).unwrap());
    out
}

fn contents(root: &Path) -> impl Iterator<Item = String> {
    files_of(root)
        .into_values()
        .map(|t| hash_bytes(t.as_bytes()))
}

/// Of `contents` a folder held when an upload looked at it, what reached
/// the hub. The rest (a held path, a failed upload) was never synced: the
/// user may replace it, and an apply keeps it (the apply ops count it).
fn committed(w: &World, contents: Vec<String>) -> Vec<String> {
    let hub = hub_contents(w);
    contents.into_iter().filter(|h| hub.contains(h)).collect()
}

/// An apply never loses what the folder held: each version is still on
/// disk (at its name or in a conflict copy) or kept by the hub.
fn kept_by_apply(w: &World, c: &Copy, before: &[String]) {
    let mut kept = hub_contents(w);
    kept.extend(contents(Path::new(&c.key)));
    for h in before {
        assert!(kept.contains(h), "an apply lost a local version");
    }
}

/// What a writer's folder holds that syncs: once c.txt is left out, its
/// later versions are the folder's own and may be replaced freely.
fn synced_contents(root: &Path, excluded: bool) -> impl Iterator<Item = String> {
    files_of(root)
        .into_iter()
        .filter(move |(p, _)| !(excluded && p.eq_ignore_ascii_case("c.txt")))
        .map(|(_, t)| hash_bytes(t.as_bytes()))
}

/// Commit `path` with `data` as a machine that is not one of the writers.
fn foreign_put(w: &World, path: &str, data: &[u8]) -> bool {
    let src = w.dir.path().join("src");
    std::fs::write(&src, data).unwrap();
    w.hub
        .import(&hash_bytes(data), &src, data.len() as u64)
        .unwrap();
    let base = w
        .index()
        .into_iter()
        .find(|e| e.path == path)
        .map_or(0, |e| e.version);
    let root = w.writers[0].1.root_id.clone();
    // Refused when the root is gone: fine, nothing to check then.
    w.hub
        .commit(
            "other-os",
            "other-os",
            &root,
            None,
            None,
            None,
            &[FileChange {
                path: path.into(),
                base_version: base,
                op: ChangeOp::Put {
                    hash: hash_bytes(data),
                    size: data.len() as i64,
                    mode_x: false,
                    mtime: 0,
                },
            }],
        )
        .is_ok_and(|o| o.is_ok())
}

async fn run(ops: Vec<Op>) {
    let w = world(2).await;
    let mut n = 0usize;
    // Every version that reached the hub (from a writer's folder or
    // another machine): it must survive somewhere.
    let mut seen: HashSet<String> = HashSet::new();
    let mut excluded = false;
    let mut twin = false;
    // Tombstones passed retention: a copy that never saw a delete keeps
    // the file (nothing tells it apart from a path left out on the hub).
    let mut expired = false;
    // A writer that lost its bases sees the clone's own file as new.
    let mut clone_bases_lost = false;
    for op in ops {
        w.reconcile();
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
                if let Some((e, c)) = w.writer(i) {
                    let before: Vec<String> =
                        synced_contents(Path::new(&c.key), excluded).collect();
                    copy::upload(&e, &c).await.unwrap_or_default();
                    seen.extend(committed(&w, before));
                }
            }
            Op::Apply { w: i } => {
                if let Some((e, c)) = w.writer(i) {
                    copy::upload(&e, &c).await.unwrap_or_default();
                    let before: Vec<String> =
                        synced_contents(Path::new(&c.key), excluded).collect();
                    let _ = copy::apply(&e, &c, false).await;
                    kept_by_apply(&w, &c, &before);
                    seen.extend(committed(&w, before));
                }
            }
            Op::LoseBases { w: i } => {
                clone_bases_lost |= i == 2;
                let key = &w.writers[i].1.key;
                let lost: Vec<(String, Option<Base>)> = w
                    .store
                    .file_bases(key)
                    .unwrap()
                    .into_keys()
                    .map(|p| (p, None))
                    .collect();
                w.store.update_file_bases(key, &lost).unwrap();
            }
            Op::WriteDuringUpload { w: i, f } => {
                if let Some((e, c)) = w.writer(i) {
                    n += 1;
                    let p = PathBuf::from(&c.key).join(FILES[f]);
                    // The user replaces the file while it uploads: what the
                    // scan saw was never synced and is theirs to replace.
                    let text = format!("w{i} racing write {n}");
                    *w.pending_write.lock().unwrap() = Some((p, text.clone()));
                    // Its content counts once a later pass looks at it.
                    copy::upload(&e, &c).await.unwrap_or_default();
                    let _ = text;
                }
            }
            Op::UploadThenCrash { w: i } => {
                if let Some((e, c)) = w.writer(i) {
                    let before = w.store.file_bases(&c.key).unwrap();
                    let held: Vec<String> = synced_contents(Path::new(&c.key), excluded).collect();
                    copy::upload(&e, &c).await.unwrap_or_default();
                    seen.extend(committed(&w, held));
                    let now = w.store.file_bases(&c.key).unwrap();
                    let restore: Vec<(String, Option<Base>)> = now
                        .keys()
                        .map(|p| (p.clone(), before.get(p).cloned()))
                        .collect();
                    w.store.update_file_bases(&c.key, &restore).unwrap();
                }
            }
            Op::CaseTwin => {
                n += 1;
                let text = format!("other os {n}");
                if foreign_put(&w, "C.txt", text.as_bytes()) {
                    seen.insert(hash_bytes(text.as_bytes()));
                }
                twin = true;
            }
            Op::Exclude => {
                let p = PathBuf::from(&w.writers[0].1.key).join(".blirpignore");
                std::fs::write(p, "c.txt\n").unwrap();
                excluded = true;
            }
            Op::ExpireTombstones => {
                // History stays: only deletions pass retention.
                w.store.hub_prune_files(i64::MIN, i64::MAX).unwrap();
                expired = true;
            }
            Op::DeleteRoot => {
                let root = w.writers[0].1.root_id.clone();
                if let Some(info) = w
                    .hub
                    .roots()
                    .unwrap()
                    .0
                    .into_iter()
                    .find(|r| r.root_id == root)
                {
                    w.hub.set_mode(&info.project_id, FilesMode::Off).unwrap();
                    w.hub.delete_root(&root).unwrap();
                    w.hub
                        .set_mode(&info.project_id, FilesMode::Default)
                        .unwrap();
                    // Deleting the hub copy deliberately drops its history:
                    // only what later passes see counts from now on.
                    seen.clear();
                }
            }
        }
    }
    // Quiet: everyone uploads and takes the hub's changes until nothing moves.
    // Quiet means no copy changed and the hub took nothing new in a round.
    let mut last: Vec<BTreeMap<String, String>> = Vec::new();
    let mut last_hub = BTreeMap::new();
    let mut quiet = false;
    for _ in 0..12 {
        for i in 0..w.writers.len() {
            w.reconcile();
            let Some((e, c)) = w.writer(i) else { continue };
            copy::upload(&e, &c).await.unwrap_or_default();
            let before: Vec<String> = synced_contents(Path::new(&c.key), excluded).collect();
            if let Ok(r) = copy::apply(&e, &c, false).await {
                assert!(r.failed.is_empty(), "{:?}", r.failed);
            }
            kept_by_apply(&w, &c, &before);
            seen.extend(committed(&w, before));
        }
        let index = w.index();
        let on_hub: HashSet<String> = index.iter().map(|e| e.path.clone()).collect();
        let live: HashMap<String, String> = index
            .iter()
            .filter_map(|e| match &e.content {
                Some(EntryContent::Blob { hash }) => Some((e.path.clone(), hash.clone())),
                _ => None,
            })
            .collect();
        // Live names the hub holds in more than one case (a twin and, later,
        // conflict copies of each): a case-insensitive copy holds one of
        // them, whichever it had, and holds it as the hub has it.
        let mut folded: HashMap<String, usize> = HashMap::new();
        for p in live.keys() {
            *folded.entry(p.to_lowercase()).or_default() += 1;
        }
        let ci = blirp_core::files::path::case_insensitive_fs();
        let now: Vec<BTreeMap<String, String>> = (0..w.writers.len())
            .filter_map(|i| w.writer(i))
            .map(|(_, c)| {
                let mut f = files_of(Path::new(&c.key));
                // Not synced by design: the clone's own file, c.txt with
                // its case twin once left out, and on a case-insensitive
                // system whichever of the twins a copy holds (each keeps
                // the one it had; the other stays on the hub).
                f.remove(CLONE_ONLY);
                if expired {
                    f.retain(|p, _| on_hub.contains(p));
                }
                if excluded || (twin && ci) {
                    f.remove("c.txt");
                    f.remove("C.txt");
                }
                if ci {
                    f.retain(|p, text| {
                        if folded.get(&p.to_lowercase()).is_none_or(|n| *n < 2) {
                            return true;
                        }
                        assert_eq!(
                            live.get(p),
                            Some(&hash_bytes(text.as_bytes())),
                            "{p} differs from the hub's version of that exact name"
                        );
                        false
                    });
                }
                f
            })
            .collect();
        let hub: BTreeMap<String, i64> = index.into_iter().map(|e| (e.path, e.version)).collect();
        if now == last && hub == last_hub {
            quiet = true;
            break;
        }
        last = now;
        last_hub = hub;
    }
    assert!(quiet, "copies and hub kept changing");
    // Converged: every copy still syncing holds the same files.
    for other in last.iter().skip(1) {
        assert_eq!(&last[0], other, "copies did not converge");
    }
    // Nothing observed was lost: on disk somewhere, on the hub or in history.
    let mut kept = hub_contents(&w);
    for (_, c) in &w.writers {
        kept.extend(contents(Path::new(&c.key)));
    }
    for h in &seen {
        assert!(kept.contains(h), "a synced version was lost");
    }
    // The clone's own file never reached the hub.
    if !clone_bases_lost {
        assert!(w.index().iter().all(|e| e.path != CLONE_ONLY));
    }
    // Nothing was written outside the folders or into a VCS folder.
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
        "db", "db-wal", "db-shm", "hubfiles", "w0", "w1", "w2", "dl0", "dl1", "dl2", "src",
    ];
    assert!(top.iter().all(|t| allowed.contains(&t.as_str())), "{top:?}");
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 32, ..ProptestConfig::default() })]

    #[test]
    fn writers_converge_and_lose_nothing(ops in proptest::collection::vec(op(3), 1..28)) {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(run(ops));
    }
}

#[tokio::test]
async fn receivers_refuse_unsafe_paths_and_keep_what_they_cannot_hold() {
    let w = world(1).await;
    let (e1, c1) = w.writer(1).unwrap();
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
    let r = copy::apply(&e1, &c1, false).await.unwrap();
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
        // One name is written, the other stays on the hub only.
        assert_eq!(
            r.skipped
                .iter()
                .filter(|(_, why)| why.contains("case"))
                .count(),
            1,
            "{r:?}"
        );
        assert!(r.conflicts.is_empty(), "{r:?}");
        // Applying again keeps the same name: the one held here is taken
        // before any new one is looked at.
        let again = copy::apply(&e1, &c1, false).await.unwrap();
        assert!(
            again.conflicts.is_empty() && again.written == 0,
            "{again:?}"
        );
    }
    // What this machine could not hold is never read as a local delete.
    let report = copy::upload(&e1, &c1).await.unwrap();
    assert_eq!(report.pending, 0);
    let entries = w.index();
    for name in ["CON", "Case.txt", "case.txt"] {
        assert!(
            entries
                .iter()
                .any(|e| e.path == name && e.content.is_some()),
            "{name} was deleted on the hub"
        );
    }
}

#[tokio::test]
async fn many_disappearing_files_are_held_until_confirmed_or_restored() {
    let w = world(0).await;
    let (e, c) = w.writer(0).unwrap();
    let root = PathBuf::from(&c.key);
    for i in 0..80 {
        std::fs::write(root.join(format!("f{i:02}.txt")), format!("file {i}")).unwrap();
    }
    copy::upload(&e, &c).await.unwrap();
    let live = |w: &World| w.index().iter().filter(|e| e.content.is_some()).count();
    assert_eq!(live(&w), 82);

    // A few deletes go through as usual.
    for i in 0..3 {
        std::fs::remove_file(root.join(format!("f{i:02}.txt"))).unwrap();
    }
    let r = copy::upload(&e, &c).await.unwrap();
    assert!(r.held_deletes.is_empty());
    assert_eq!(live(&w), 79);

    // 60 at once (>= 10 and > 30%): nothing is committed, not even edits.
    for i in 3..63 {
        std::fs::remove_file(root.join(format!("f{i:02}.txt"))).unwrap();
    }
    std::fs::write(root.join("f70.txt"), "edited meanwhile").unwrap();
    let r = copy::upload(&e, &c).await.unwrap();
    assert_eq!(r.held_deletes.len(), 60);
    assert_eq!(r.sent, 0);
    assert_eq!(live(&w), 79);
    // Still held on the next pass (left paused).
    let held = copy::upload(&e, &c).await.unwrap().held_deletes;
    assert_eq!(held.len(), 60);

    // Restore from hub brings exactly the missing files back.
    let restored = copy::restore_missing(&e, &c).await.unwrap();
    assert_eq!((restored.written, restored.failed.len()), (60, 0));
    assert_eq!(
        std::fs::read_to_string(root.join("f10.txt")).unwrap(),
        "file 10"
    );
    assert!(
        !root.join("f01.txt").exists(),
        "a confirmed delete stays deleted"
    );
    let r = copy::upload(&e, &c).await.unwrap();
    assert!(r.held_deletes.is_empty());
    assert_eq!(r.sent, 1, "the edit uploads once the folder is whole again");

    // Delete on hub too covers what was shown: a file that disappears
    // after that waits for its own confirmation.
    for i in 3..63 {
        std::fs::remove_file(root.join(format!("f{i:02}.txt"))).unwrap();
    }
    let shown: HashSet<String> = copy::upload(&e, &c)
        .await
        .unwrap()
        .held_deletes
        .into_iter()
        .collect();
    assert_eq!(shown.len(), 60);
    std::fs::remove_file(root.join("f70.txt")).unwrap();
    let r = copy::upload_with(&e, &c, copy::Deletes::Confirm(shown))
        .await
        .unwrap();
    assert_eq!(r.sent, 60);
    assert_eq!(live(&w), 19);
    assert!(
        w.index()
            .iter()
            .any(|x| x.path == "f70.txt" && x.content.is_some())
    );
    let r = copy::upload(&e, &c).await.unwrap();
    assert_eq!(r.sent, 1, "one more delete, confirmed by nobody, is small");

    // Emptying a small folder is held too.
    let small = world(0).await;
    let (se, sc) = small.writer(0).unwrap();
    for f in files_of(Path::new(&sc.key)).keys() {
        std::fs::remove_file(PathBuf::from(&sc.key).join(f)).unwrap();
    }
    assert_eq!(copy::upload(&se, &sc).await.unwrap().held_deletes.len(), 2);

    // Restoring needs the folder itself.
    std::fs::remove_dir_all(&root).unwrap();
    assert!(copy::restore_missing(&e, &c).await.is_err());
}

#[tokio::test]
async fn a_hub_copy_made_again_is_uploaded_whole_and_copies_detach() {
    let w = world(1).await;
    w.reconcile();
    let (e0, c0) = w.writer(0).unwrap();
    assert!(!c0.incarnation.is_empty());
    let old_head = w.hub.roots().unwrap().0[0].head;
    // Delete the hub copy; the origin makes it again with its next upload.
    let project = w.hub.roots().unwrap().0[0].project_id.clone();
    w.hub.set_mode(&project, FilesMode::Off).unwrap();
    w.hub.delete_root(&c0.root_id).unwrap();
    w.hub.set_mode(&project, FilesMode::Default).unwrap();
    w.reconcile();
    // The copy detached; the origin forgot its bases.
    assert!(w.writer(1).is_none());
    let (e0, c0) = w.writer(0).map_or((e0, c0), |x| x);
    assert!(w.store.file_bases(&c0.key).unwrap().is_empty());
    copy::upload(&e0, &c0).await.unwrap();
    let info = w.hub.roots().unwrap().0[0].clone();
    // Whole, never partial, and the sequence continued.
    let names: HashSet<String> = w.index().into_iter().map(|e| e.path).collect();
    assert_eq!(names, HashSet::from(["seed.txt".into(), "a.txt".into()]));
    assert!(info.head > old_head, "{} <= {old_head}", info.head);
    w.reconcile();
    assert_eq!(w.writer(0).unwrap().1.incarnation, info.incarnation);
    // A copy of the old one cannot read the new one as its own.
    let (_, c1) = &w.writers[1];
    let stale = Copy {
        incarnation: "old".into(),
        ..c1.clone()
    };
    let err = copy::apply(&w.writers[1].0, &stale, false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "root_replaced");
}

#[tokio::test]
async fn a_fresh_copy_takes_the_hub_over_its_clone_and_uploads_none_of_it() {
    let w = world(2).await;
    let (e2, c2) = w.writer(2).unwrap();
    let root2 = PathBuf::from(&c2.key);
    // The hub's version replaced the clone's; the clone's own file stayed.
    assert_eq!(
        std::fs::read_to_string(root2.join("a.txt")).unwrap(),
        "origin a"
    );
    assert!(root2.join(CLONE_ONLY).exists());
    let r = copy::upload(&e2, &c2).await.unwrap();
    assert_eq!((r.sent, r.conflicts), (0, 0), "{r:?}");
    assert!(w.index().iter().all(|e| e.path != CLONE_ONLY));
    // A tombstoned file of the clone is removed by the fresh overlay.
    let (e0, c0) = w.writer(0).unwrap();
    std::fs::remove_file(PathBuf::from(&c0.key).join("a.txt")).unwrap();
    copy::upload(&e0, &c0).await.unwrap();
    let dir = w.dir.path().join("w3");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.txt"), "clone still has it").unwrap();
    let key = dunce::canonicalize(&dir).unwrap().display().to_string();
    let inc = w.hub.roots().unwrap().0[0].incarnation.clone();
    w.store
        .put_file_copy(&FileCopy {
            path: key.clone(),
            root_id: c0.root_id.clone(),
            origin: false,
            seen: 0,
            created_at: 0,
            mode: CopyMode::Pending,
            incarnation: inc.clone(),
        })
        .unwrap();
    let c3 = Copy {
        key,
        root_id: c0.root_id.clone(),
        origin: false,
        incarnation: inc,
    };
    copy::apply(&e2, &c3, true).await.unwrap();
    assert!(!dir.join("a.txt").exists());
}

#[tokio::test]
async fn forgotten_and_expired_paths_are_not_uploaded_again() {
    let w = world(1).await;
    let (e0, c0) = w.writer(0).unwrap();
    let (e1, c1) = w.writer(1).unwrap();
    // The origin leaves a.txt out: forgotten on the hub, kept everywhere.
    std::fs::write(PathBuf::from(&c0.key).join(".blirpignore"), "a.txt\n").unwrap();
    copy::upload(&e0, &c0).await.unwrap();
    assert!(w.index().iter().all(|e| e.path != "a.txt"));
    // The copy takes the change; its unchanged a.txt stays and is not
    // uploaded again as new.
    copy::apply(&e1, &c1, false).await.unwrap();
    assert!(PathBuf::from(&c1.key).join("a.txt").exists());
    std::fs::remove_file(PathBuf::from(&c1.key).join(".blirpignore")).unwrap();
    // (Its own copy of the ignore file gone, a.txt would be a candidate.)
    let r = copy::upload(&e1, &c1).await.unwrap();
    assert!(
        w.index()
            .iter()
            .all(|e| e.path != "a.txt" || e.content.is_none()),
        "{r:?}"
    );
    // A deletion past retention does not come back either.
    std::fs::write(PathBuf::from(&c0.key).join("gone.txt"), "x").unwrap();
    copy::upload(&e0, &c0).await.unwrap();
    copy::apply(&e1, &c1, false).await.unwrap();
    std::fs::remove_file(PathBuf::from(&c0.key).join("gone.txt")).unwrap();
    copy::upload(&e0, &c0).await.unwrap();
    w.store.hub_prune_files(i64::MIN, i64::MAX).unwrap();
    let copy_file = PathBuf::from(&c1.key).join("gone.txt");
    assert!(copy_file.exists());
    copy::apply(&e1, &c1, false).await.unwrap();
    copy::upload(&e1, &c1).await.unwrap();
    assert!(w.index().iter().all(|e| e.path != "gone.txt"));
}

#[tokio::test]
async fn a_copy_racing_a_remade_root_detaches_without_committing() {
    let w = world(1).await;
    let (e0, c0) = w.writer(0).unwrap();
    // Taken before the hub copy is deleted and made again: its bases and
    // incarnation belong to the old one, and no reconcile runs between.
    let (e1, c1) = w.writer(1).unwrap();
    let project = w.hub.roots().unwrap().0[0].project_id.clone();
    w.hub.set_mode(&project, FilesMode::Off).unwrap();
    w.hub.delete_root(&c0.root_id).unwrap();
    w.hub.set_mode(&project, FilesMode::Default).unwrap();
    // The origin notices at its next commit and starts over.
    std::fs::write(Path::new(&c0.key).join("a.txt"), "origin edit").unwrap();
    let err = copy::upload(&e0, &c0).await.unwrap_err();
    assert_eq!(err.code(), "root_replaced");
    let (e0, c0) = w.writer(0).unwrap();
    assert!(c0.incarnation.is_empty());
    copy::upload(&e0, &c0).await.unwrap();
    let (_, c0) = w.writer(0).unwrap();
    assert!(!c0.incarnation.is_empty() && c0.incarnation != c1.incarnation);
    // The copy's edit is refused before anything is applied, and it
    // detaches with its files kept.
    std::fs::write(Path::new(&c1.key).join("late.txt"), "late edit").unwrap();
    let err = copy::upload(&e1, &c1).await.unwrap_err();
    assert_eq!(err.code(), "root_replaced");
    assert!(w.writer(1).is_none());
    assert!(w.index().iter().all(|e| e.path != "late.txt"));
    assert!(Path::new(&c1.key).join("late.txt").exists());
}

#[tokio::test]
async fn a_file_over_the_hubs_limit_does_not_hold_up_its_folder() {
    let w = world(0).await;
    let (e0, c0) = w.writer(0).unwrap();
    w.hub.set_max_file(100);
    std::fs::write(Path::new(&c0.key).join("big.bin"), vec![b'x'; 1000]).unwrap();
    std::fs::write(Path::new(&c0.key).join("small.txt"), "small").unwrap();
    let r = copy::upload(&e0, &c0).await.unwrap();
    assert_eq!(r.sent, 1, "{r:?}");
    let paths: Vec<String> = w.index().into_iter().map(|e| e.path).collect();
    assert!(paths.contains(&"small.txt".to_string()));
    assert!(!paths.contains(&"big.bin".to_string()));
    // The hub itself refuses it too, whatever a node's settings say.
    let src = w.dir.path().join("big-src");
    std::fs::write(&src, vec![b'x'; 1000]).unwrap();
    let err = w
        .hub
        .import(&hash_bytes(&[b'x'; 1000]), &src, 1000)
        .unwrap_err();
    assert_eq!(err.code(), "too_large");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_write_interrupted_after_moving_a_file_aside_is_not_a_delete() {
    let w = world(0).await;
    let (e0, c0) = w.writer(0).unwrap();
    let root = Path::new(&c0.key);
    // The state a crash leaves between moving the target aside and moving
    // the new file in.
    std::fs::rename(
        root.join("a.txt"),
        root.join(format!("{}a.txt", blirp_core::files::write::ASIDE_PREFIX)),
    )
    .unwrap();
    let r = copy::upload(&e0, &c0).await.unwrap();
    assert_eq!((r.sent, r.held_deletes.len()), (0, 0), "{r:?}");
    assert_eq!(
        std::fs::read_to_string(root.join("a.txt")).unwrap(),
        "origin a"
    );
    assert!(
        w.index()
            .iter()
            .any(|e| e.path == "a.txt" && e.content.is_some())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_returned_folder_holds_even_a_single_delete() {
    let w = world(0).await;
    let (e0, c0) = w.writer(0).unwrap();
    std::fs::remove_file(Path::new(&c0.key).join("a.txt")).unwrap();
    // One of two files: no mass delete, but a folder that was just missing
    // holds it all the same.
    let r = copy::upload_with(&e0, &c0, copy::Deletes::HoldAll)
        .await
        .unwrap();
    assert_eq!(r.held_deletes, ["a.txt"]);
    assert!(
        w.index()
            .iter()
            .any(|e| e.path == "a.txt" && e.content.is_some())
    );
    let r = copy::upload(&e0, &c0).await.unwrap();
    assert!(r.held_deletes.is_empty());
    assert!(
        w.index()
            .iter()
            .any(|e| e.path == "a.txt" && e.content.is_none())
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_empty_file_syncs_with_the_rest_of_its_folder() {
    let w = world(1).await;
    let (e0, c0) = w.writer(0).unwrap();
    std::fs::write(Path::new(&c0.key).join("empty.txt"), "").unwrap();
    std::fs::write(Path::new(&c0.key).join("next.txt"), "next").unwrap();
    let r = copy::upload(&e0, &c0).await.unwrap();
    assert_eq!((r.sent, r.pending), (2, 0), "{r:?}");
    let (e1, c1) = w.writer(1).unwrap();
    let a = copy::apply(&e1, &c1, false).await.unwrap();
    assert!(a.failed.is_empty(), "{:?}", a.failed);
    let files = files_of(Path::new(&c1.key));
    assert_eq!(files.get("empty.txt").map(String::as_str), Some(""));
    assert_eq!(files.get("next.txt").map(String::as_str), Some("next"));
}
