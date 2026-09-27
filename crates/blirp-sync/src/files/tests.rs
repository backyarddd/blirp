//! `blirp/files/1` over real loopback iroh endpoints (`BLIRP_LOOPBACK_ONLY`
//! is set by `.cargo/config.toml`): resumable transfers in both directions,
//! hash verification, commits, notifications, and a hub without the ALPN.

use super::blobs::BlobStore;
use super::client::{Connector, FileHub, HUB_OUTDATED, RemoteHub};
use super::hub::HubFiles;
use super::proto::{CHUNK, Notify};
use crate::service::bind;
use crate::{ALPN_FILES, ALPN_SYNC, SyncError};
use blirp_core::files::{ChangeOp, ChangeResult, FileChange, hash_bytes};
use blirp_core::store::Store;
use iroh::{Endpoint, SecretKey};
use std::sync::{Arc, Mutex};

struct Rig {
    _dir: tempfile::TempDir,
    hub: Arc<HubFiles>,
    hub_ep: Endpoint,
    node_ep: Endpoint,
    remote: Arc<RemoteHub>,
    root: String,
    folder: String,
    notes: Arc<Mutex<Vec<Notify>>>,
    parts: BlobStore,
    dir: std::path::PathBuf,
}

async fn rig(alpn: &'static [u8]) -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let hub_ep = bind(
        &SecretKey::from_bytes(&[1; 32]),
        "disabled",
        false,
        vec![alpn.to_vec()],
        true,
    )
    .await
    .unwrap()
    .0;
    let node_ep = bind(
        &SecretKey::from_bytes(&[2; 32]),
        "disabled",
        false,
        Vec::new(),
        false,
    )
    .await
    .unwrap()
    .0;
    let node_id = node_ep.id().to_string();
    let store = Arc::new(Store::open(&dir.path().join("db")).unwrap());
    let folder = dir.path().join("proj");
    std::fs::create_dir(&folder).unwrap();
    let p = store.register_project(&node_id, &folder, None).unwrap();
    let folder = store.project_paths(&p.id).unwrap()[0].path.clone();
    let hub = Arc::new(HubFiles::new(
        store,
        &dir.path().join("hubfiles"),
        1 << 30,
        1000,
    ));
    let (h, ep) = (hub.clone(), hub_ep.clone());
    tokio::spawn(async move {
        while let Some(inc) = ep.accept().await {
            let h = h.clone();
            tokio::spawn(async move {
                let Ok(conn) = inc.await else { return };
                let id = conn.remote_id().to_string();
                let _ = super::server::serve(conn, h, id, "lap top".into()).await;
            });
        }
    });
    let addr = hub_ep.addr();
    let ep = node_ep.clone();
    let connect: Connector = Arc::new(move || {
        let (ep, addr) = (ep.clone(), addr.clone());
        Box::pin(async move {
            ep.connect(addr, ALPN_FILES)
                .await
                .map_err(|e| SyncError::Connect {
                    what: "the hub".into(),
                    message: e.to_string(),
                })
        })
    });
    let notes: Arc<Mutex<Vec<Notify>>> = Arc::default();
    let n = notes.clone();
    let parts = BlobStore::new(dir.path().join("dl"));
    let remote = Arc::new(RemoteHub::new(
        connect,
        parts.clone(),
        0,
        Arc::new(move |x| n.lock().unwrap().push(x)),
    ));
    Rig {
        root: blirp_core::files::root_id(&node_id, &folder),
        dir: dir.path().to_path_buf(),
        _dir: dir,
        hub,
        hub_ep,
        node_ep,
        remote,
        folder,
        notes,
        parts,
    }
}

fn noise(seed: &[u8], n: usize) -> Vec<u8> {
    let mut v = vec![0u8; n];
    blake3::Hasher::new()
        .update(seed)
        .finalize_xof()
        .fill(&mut v);
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn uploads_commits_downloads_and_resumes() {
    let r = rig(ALPN_FILES).await;
    let fh = FileHub::Remote(r.remote.clone());
    let w = fh.welcome().await.unwrap();
    assert_eq!(w.quota, 1 << 30);

    // A 2.5-chunk file whose first chunk already reached the hub.
    let data = noise(b"big", CHUNK * 5 / 2);
    let h = hash_bytes(&data);
    r.hub.begin_put(&h, data.len() as u64).unwrap();
    r.hub.put_chunk(&h, 0, &data[..CHUNK]).unwrap();
    let missing = fh.missing(std::slice::from_ref(&h)).await.unwrap();
    assert_eq!(
        (missing[0].hash.as_str(), missing[0].have),
        (h.as_str(), CHUNK as u64)
    );
    let src = r.dir.join("src");
    std::fs::write(&src, &data).unwrap();
    fh.upload(&h, &src, data.len() as u64).await.unwrap();
    assert!(
        fh.missing(std::slice::from_ref(&h))
            .await
            .unwrap()
            .is_empty()
    );

    // Content that does not match its announced hash is refused.
    let wrong = hash_bytes(b"something else");
    let err = fh
        .upload(&wrong, &src, data.len() as u64)
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SyncError::Remote { code, .. } if code == "hash_mismatch"),
        "{err}"
    );

    // Commit (registering the root from its origin) and get notified.
    let out = fh
        .commit(
            &r.root,
            Some(&r.folder),
            None,
            vec![FileChange {
                path: "data/big.bin".into(),
                base_version: 0,
                op: ChangeOp::Put {
                    hash: h.clone(),
                    size: data.len() as i64,
                    mode_x: false,
                    mtime: 5,
                },
            }],
        )
        .await
        .unwrap();
    assert_eq!(out.results, [ChangeResult::Ok { version: 1 }]);
    let (roots, _) = fh.roots().await.unwrap();
    assert_eq!(roots[0].files, 1);
    let index = fh.index(&r.root, 0).await.unwrap();
    assert_eq!((index.entries.len(), index.head), (1, 1));
    assert_eq!(index.incarnation, out.incarnation);
    for _ in 0..100 {
        if !r.notes.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert_eq!(r.notes.lock().unwrap()[0].head, 1);

    // A download resumes from what arrived before.
    r.parts.append(&h, 0, &data[..1000]).unwrap();
    let got = fh.download(&h).await.unwrap();
    assert_eq!(std::fs::read(&got).unwrap(), data);
    std::fs::remove_file(&got).unwrap();
    // A corrupt partial download is detected and dropped; the retry works.
    r.parts.append(&h, 0, &[0u8; 10]).unwrap();
    let err = fh.download(&h).await.unwrap_err();
    assert!(
        matches!(&err, SyncError::Remote { code, .. } if code == "hash_mismatch"),
        "{err}"
    );
    assert_eq!(std::fs::read(fh.download(&h).await.unwrap()).unwrap(), data);

    // A commit naming a blob the hub lacks is rejected per change.
    let out = fh
        .commit(
            &r.root,
            None,
            None,
            vec![FileChange {
                path: "x".into(),
                base_version: 0,
                op: ChangeOp::Put {
                    hash: hash_bytes(b"never uploaded"),
                    size: 1,
                    mode_x: false,
                    mtime: 0,
                },
            }],
        )
        .await
        .unwrap();
    assert!(
        matches!(&out.results[0], ChangeResult::Rejected { code, .. } if code == "missing_blob")
    );
    r.node_ep.close().await;
    r.hub_ep.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hub_without_file_sync_is_reported() {
    let r = rig(ALPN_SYNC).await;
    let err = FileHub::Remote(r.remote.clone())
        .welcome()
        .await
        .unwrap_err();
    assert!(
        matches!(&err, SyncError::Remote { code, .. } if code == HUB_OUTDATED),
        "{err}"
    );
    r.node_ep.close().await;
    r.hub_ep.close().await;
}
