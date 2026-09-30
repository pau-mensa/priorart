use priorart::{config::Settings, service::Service};
use std::time::Duration;

fn settings(path: &std::path::Path) -> Settings {
    Settings {
        data_dir: path.into(),
        ..Settings::default()
    }
}

#[test]
fn ownership_helper() {
    let Some(path) = std::env::var_os("PRIORART_OWNER_TEST_DIR") else {
        return;
    };
    let path = std::path::Path::new(&path);
    let _service = Service::open(settings(path)).unwrap();
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
    let rejected = Service::open(settings(directory.path())).is_err();
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(ready && rejected);
    let service = Service::open(settings(directory.path())).unwrap();
    assert!(Service::open(settings(directory.path())).is_err());
    #[cfg(unix)]
    {
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(directory.path(), &alias).unwrap();
        assert!(Service::open(settings(&alias)).is_err());
    }
    drop(service);
    assert!(Service::open(settings(directory.path())).is_ok());
}
