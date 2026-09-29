mod common;
use lateweave::{Representation, TokenMatrix};
use priorart::{
    auth::{Grant, Operation as Op, RequestContext},
    config::Settings,
    encoder::{Encoder, EncoderError},
    service::{DeleteOptions, Service, ServiceError, WriteOptions, DATABASE_FILE},
    store::{Store, Visibility, LOCAL_PRINCIPAL_ID},
};
use std::{
    sync::{mpsc, Arc, Condvar, Mutex},
    time::Duration,
};

fn settings(path: &std::path::Path, capacity: usize) -> Settings {
    Settings {
        data_dir: path.into(),
        max_loaded_indexes: capacity,
        ..Settings::default()
    }
}

#[test]
fn ownership_helper() {
    let Some(path) = std::env::var_os("PRIORART_OWNER_TEST_DIR") else {
        return;
    };
    let path = std::path::Path::new(&path);
    let _service = Service::new(settings(path, 2), None).unwrap();
    std::fs::write(path.join("ready"), "ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
fn second_writer_is_rejected_and_a_killed_process_releases_ownership() {
    let directory = tempfile::tempdir().unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "ownership_helper", "--nocapture"])
        .env("PRIORART_OWNER_TEST_DIR", directory.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while !directory.path().join("ready").exists() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(10));
    }
    let ready = directory.path().join("ready").exists();
    let rejected = Service::new(settings(directory.path(), 2), None).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(ready && rejected);
    let service = Service::new(settings(directory.path(), 2), None).unwrap();
    assert!(Service::new(settings(directory.path(), 2), None).is_err());
    #[cfg(unix)]
    {
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
        assert!(Service::new(settings(&alias, 2), None).is_err());
    }
    drop(service);
    assert!(Service::new(settings(directory.path(), 2), None).is_ok());
}

struct GatedEncoder {
    fake: common::FakeEncoder,
    entered: mpsc::Sender<()>,
    release: (Mutex<bool>, Condvar),
}
impl GatedEncoder {
    fn unblock(&self) {
        *self.release.0.lock().unwrap() = true;
        self.release.1.notify_all();
    }
}
impl Encoder for GatedEncoder {
    fn representation(&self) -> &Representation {
        self.fake.representation()
    }
    fn encode_documents(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        self.fake.encode_documents(texts)
    }
    fn encode_queries(&self, texts: &[&str]) -> Result<Vec<TokenMatrix>, EncoderError> {
        if texts.iter().any(|text| text.contains("slow")) {
            self.entered.send(()).unwrap();
            let mut released = self.release.0.lock().unwrap();
            while !*released {
                released = self.release.1.wait(released).unwrap();
            }
        }
        self.fake.encode_queries(texts)
    }
    fn fit_document<'a>(&self, text: &'a str) -> Result<&'a str, EncoderError> {
        self.fake.fit_document(text)
    }
}

#[test]
fn collections_run_independently_while_same_collection_mutations_wait() {
    for capacity in [1, 2] {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join(DATABASE_FILE)).unwrap();
        let a = store
            .create_collection(LOCAL_PRINCIPAL_ID, Visibility::Restricted)
            .unwrap();
        let b = store
            .create_collection(LOCAL_PRINCIPAL_ID, Visibility::Restricted)
            .unwrap();
        let grants: Vec<_> = [&a, &b]
            .into_iter()
            .flat_map(|id| [Op::Read, Op::Contribute, Op::Delete].map(|op| Grant::new(id, op)))
            .collect();
        let key = store
            .issue_local_credential(LOCAL_PRINCIPAL_ID, &grants, None)
            .unwrap()
            .into_secret();
        let (entered, waiting) = mpsc::channel();
        let encoder = Arc::new(GatedEncoder {
            fake: common::FakeEncoder::new(),
            entered,
            release: (Mutex::new(false), Condvar::new()),
        });
        let service = Arc::new(
            Service::new(settings(directory.path(), capacity), Some(encoder.clone())).unwrap(),
        );
        let context = service.authenticate(&key).unwrap();
        service
            .put(
                &context,
                &a,
                "alpha sentinel",
                None,
                Some("same"),
                WriteOptions::default(),
            )
            .unwrap();
        let searching = {
            let service = service.clone();
            let context = context.clone();
            let a = a.clone();
            std::thread::spawn(move || {
                service.search(&context, &a, "slow alpha sentinel", None, 10)
            })
        };
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done, completed) = mpsc::channel();
        let other = {
            let service = service.clone();
            let context = context.clone();
            let b = b.clone();
            std::thread::spawn(move || {
                done.send(service.put(
                    &context,
                    &b,
                    "beta sentinel",
                    None,
                    Some("same"),
                    WriteOptions::default(),
                ))
                .unwrap()
            })
        };
        let independent = completed.recv_timeout(Duration::from_secs(5));
        let (deleted, deletion) = mpsc::channel();
        let deleting = {
            let service = service.clone();
            let context = context.clone();
            let a = a.clone();
            std::thread::spawn(move || {
                deleted
                    .send(service.delete(
                        &context,
                        &a,
                        "same",
                        DeleteOptions {
                            expected_revision: Some(1),
                            idempotency_key: Some("delete"),
                        },
                    ))
                    .unwrap()
            })
        };
        let blocked = deletion.recv_timeout(Duration::from_millis(100)).is_err();
        encoder.unblock();
        let searched = searching.join().unwrap().unwrap();
        other.join().unwrap();
        deleting.join().unwrap();
        assert!(blocked);
        let independent = independent.expect("another collection must not wait on the first");
        if capacity == 1 {
            assert!(matches!(independent, Err(ServiceError::Busy)));
        } else {
            independent.unwrap();
        }
        deletion
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(searched.hits[0].revision, 1);
        assert_eq!(searched.hits[0].excerpt, "alpha sentinel");
        assert!(service
            .search(&context, &a, "alpha", None, 10)
            .unwrap()
            .hits
            .is_empty());
        assert!(service.authenticate(&key).is_ok());
    }
}

#[test]
fn generation_activation_keeps_the_previous_vectors_unchanged() {
    let directory = tempfile::tempdir().unwrap();
    let service = Service::new(
        settings(directory.path(), 2),
        Some(Arc::new(common::FakeEncoder::new())),
    )
    .unwrap();
    let caller = RequestContext::local();
    service
        .put(
            &caller,
            "local",
            "original sentinel",
            None,
            Some("record"),
            WriteOptions::default(),
        )
        .unwrap();
    let previous = common::active_index_path(directory.path(), "local");
    let original = std::fs::read(previous.join("manifest.json")).unwrap();
    let original_vectors = std::fs::read(previous.join("vectors/storage.json")).unwrap();
    service
        .put(
            &caller,
            "local",
            "replacement sentinel",
            None,
            Some("record"),
            WriteOptions {
                expected_revision: Some(1),
                ..WriteOptions::default()
            },
        )
        .unwrap();
    let current = common::active_index_path(directory.path(), "local");
    assert_ne!(previous, current);
    assert_eq!(
        std::fs::read(previous.join("manifest.json")).unwrap(),
        original
    );
    assert_eq!(
        std::fs::read(previous.join("vectors/storage.json")).unwrap(),
        original_vectors
    );
    drop(service);
    let service = Service::new(
        settings(directory.path(), 2),
        Some(Arc::new(common::FakeEncoder::new())),
    )
    .unwrap();
    let hits = service
        .search(&caller, "local", "replacement", None, 10)
        .unwrap()
        .hits;
    assert_eq!(hits[0].revision, 2);
    assert_eq!(
        common::active_index_path(directory.path(), "local"),
        current
    );
    assert!(previous.exists());
}
