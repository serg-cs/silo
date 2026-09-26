use std::fs;
use std::path::Path;
use std::process::Command;
use std::thread;
use std::time::{Duration, Instant};

use crate::image::runtime_contract::LIFECYCLE;
use crate::test_support::test_dir;

#[cfg(target_os = "linux")]
#[test]
fn guest_lifecycle_reserves_runs_and_stops_after_idle() {
    let dir = test_dir("guest-lifecycle");
    let runtime = dir.path().join("runtime");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(runtime.join("reservations")).expect("reservation directory creates");
    fs::write(runtime.join("ready"), "").expect("readiness marker creates");

    let mut supervisor = Command::new("sh")
        .arg(&script)
        .arg("init")
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("supervisor starts");
    let ready = runtime.join("ready");
    let deadline = Instant::now() + Duration::from_secs(2);
    while !ready.exists() {
        assert!(Instant::now() < deadline, "guest did not become ready");
        thread::sleep(Duration::from_millis(10));
    }

    let reservation = Command::new("sh")
        .arg(&script)
        .arg("reserve")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("reservation runs");
    assert!(reservation.status.success());
    let token = String::from_utf8(reservation.stdout)
        .expect("token is UTF-8")
        .trim()
        .to_string();
    assert!(!token.is_empty());

    let status = Command::new("sh")
        .arg(&script)
        .args(["session", &token, "true"])
        .env("SILO_RUNTIME_DIR", &runtime)
        .status()
        .expect("session runs");
    assert!(status.success());

    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if supervisor.try_wait().expect("supervisor polls").is_some() {
            break;
        }
        assert!(Instant::now() < deadline, "guest did not stop after idle");
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guest_lifecycle_blocks_stop_while_sessions_overlap() {
    let dir = test_dir("guest-overlap");
    let runtime = dir.path().join("runtime");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(runtime.join("reservations")).expect("reservation directory creates");

    let mut sessions = Vec::new();
    let release = runtime.join("release");
    for index in 0..2 {
        let reservation = Command::new("sh")
            .arg(&script)
            .arg("reserve")
            .env("SILO_RUNTIME_DIR", &runtime)
            .output()
            .expect("reservation runs");
        assert!(reservation.status.success());
        let token = String::from_utf8(reservation.stdout)
            .expect("token is UTF-8")
            .trim()
            .to_string();
        let active = runtime.join(format!("active-{index}"));
        let session = Command::new("sh")
            .arg(&script)
            .args(["session", &token, "sh", "-c"])
            .arg("touch \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.01; done")
            .arg("session")
            .arg(&active)
            .arg(&release)
            .env("SILO_RUNTIME_DIR", &runtime)
            .spawn()
            .expect("session starts");
        sessions.push((session, active));
    }

    let deadline = Instant::now() + Duration::from_secs(2);
    while sessions.iter().any(|(_, active)| !active.exists()) {
        assert!(Instant::now() < deadline, "sessions did not become active");
        thread::sleep(Duration::from_millis(10));
    }
    let guarded = Command::new("sh")
        .arg(&script)
        .arg("stop-guard")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("stop guard runs");
    assert_eq!(guarded.status.code(), Some(75));

    fs::write(&release, "").expect("sessions release");
    for (mut session, _) in sessions {
        assert!(session.wait().expect("session exits").success());
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guest_lifecycle_persistence_keeps_an_idle_container_running() {
    let dir = test_dir("guest-persistence");
    let runtime = dir.path().join("runtime");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(runtime.join("reservations")).expect("reservation directory creates");

    let mut supervisor = Command::new("sh")
        .arg(&script)
        .arg("init")
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("supervisor starts");
    let reservation = Command::new("sh")
        .arg(&script)
        .arg("reserve")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("reservation runs");
    assert!(reservation.status.success());
    let token = String::from_utf8(reservation.stdout)
        .expect("token is UTF-8")
        .trim()
        .to_string();
    let persisted = Command::new("sh")
        .arg(&script)
        .args(["persist", &token])
        .env("SILO_RUNTIME_DIR", &runtime)
        .status()
        .expect("persistence handoff runs");
    assert!(persisted.success());

    thread::sleep(Duration::from_millis(250));
    assert!(
        supervisor.try_wait().expect("supervisor polls").is_none(),
        "persistent supervisor exited while idle"
    );

    fs::remove_file(runtime.join("persistent")).expect("persistence marker removes");
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        if supervisor.try_wait().expect("supervisor polls").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "supervisor did not exit after unpinning"
        );
        thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(target_os = "linux")]
#[test]
fn guest_reserve_replaces_the_previous_token() {
    let dir = test_dir("guest-renew");
    let runtime = dir.path().join("runtime");
    let reservations = runtime.join("reservations");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(&reservations).expect("reservation directory creates");

    let first = Command::new("sh")
        .arg(&script)
        .arg("reserve")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("first reservation runs");
    assert!(first.status.success());
    let previous = String::from_utf8(first.stdout)
        .expect("token is UTF-8")
        .trim()
        .to_string();
    assert!(reservations.join(&previous).is_file());

    let renewed = Command::new("sh")
        .arg(&script)
        .args(["reserve", &previous])
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("renewed reservation runs");
    assert!(renewed.status.success());
    let current = String::from_utf8(renewed.stdout)
        .expect("token is UTF-8")
        .trim()
        .to_string();
    assert_ne!(current, previous);
    assert!(!reservations.join(&previous).exists());
    assert!(reservations.join(&current).is_file());

    let released = Command::new("sh")
        .arg(&script)
        .args(["release", &current])
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("release runs");
    assert!(released.status.success());
    assert!(!reservations.join(&current).exists());
}

#[cfg(target_os = "linux")]
#[test]
fn guest_release_does_not_wait_for_a_joined_session() {
    let dir = test_dir("guest-release-during-session");
    let runtime = dir.path().join("runtime");
    let reservations = runtime.join("reservations");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(&reservations).expect("reservation directory creates");

    let session_token = reserve_token(&script, &runtime);
    let held = runtime.join("held");
    let release_flag = runtime.join("release-session");
    let mut session = Command::new("sh")
        .arg(&script)
        .args(["session", &session_token, "sh", "-c"])
        .arg("touch \"$1\"; while [ ! -e \"$2\" ]; do sleep 0.05; done")
        .arg("session")
        .arg(&held)
        .arg(&release_flag)
        .env("SILO_RUNTIME_DIR", &runtime)
        .spawn()
        .expect("session starts");
    let ready = Instant::now() + Duration::from_secs(2);
    while !held.exists() {
        assert!(Instant::now() < ready, "session did not take the lock");
        thread::sleep(Duration::from_millis(10));
    }

    let creator = reserve_token(&script, &runtime);
    let started = Instant::now();
    let released = Command::new("sh")
        .arg(&script)
        .args(["release", &creator])
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("release runs");
    assert!(released.status.success());
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!reservations.join(&creator).exists());

    fs::write(&release_flag, "").expect("session releases");
    let _ = session.wait();
}

#[cfg(target_os = "linux")]
fn reserve_token(script: &Path, runtime: &Path) -> String {
    let reservation = Command::new("sh")
        .arg(script)
        .arg("reserve")
        .env("SILO_RUNTIME_DIR", runtime)
        .output()
        .expect("reservation runs");
    assert!(reservation.status.success());
    String::from_utf8(reservation.stdout)
        .expect("token is UTF-8")
        .trim()
        .to_string()
}

#[cfg(target_os = "linux")]
#[test]
fn guest_stop_guard_rejects_live_reservations_and_prunes_expired_ones() {
    let dir = test_dir("guest-reservations");
    let runtime = dir.path().join("runtime");
    let reservations = runtime.join("reservations");
    let script = dir.path().join("silo-lifecycle");
    fs::write(&script, LIFECYCLE).expect("lifecycle script writes");
    fs::create_dir_all(&reservations).expect("reservation directory creates");

    fs::write(reservations.join("live"), "9999999999\n").expect("live reservation writes");
    let blocked = Command::new("sh")
        .arg(&script)
        .arg("stop-guard")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("stop guard runs");
    assert_eq!(blocked.status.code(), Some(76));

    fs::remove_file(reservations.join("live")).expect("live reservation removes");
    let expired = reservations.join("expired");
    fs::write(&expired, "0\n").expect("expired reservation writes");
    let ready = Command::new("sh")
        .arg(&script)
        .arg("stop-guard")
        .env("SILO_RUNTIME_DIR", &runtime)
        .output()
        .expect("stop guard runs");
    assert!(ready.status.success());
    assert_eq!(ready.stdout, b"ready\n");
    assert!(!expired.exists());
}
