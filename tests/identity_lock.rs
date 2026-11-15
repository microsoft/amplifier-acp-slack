//! The image-sync lock admits one update at a time, and a child process that is
//! only starting must not look like a second update. No Slack, agent or network.
//!
//! One test, so nothing else in this process starts children or holds locks.
use acp_slack_adapter::{config::Config, identity};
use std::{
    path::Path,
    time::{Duration, Instant},
};

const BUSY: &str = "An image update is already running; try again shortly";

/// With image updates turned off, a sync takes the lock and returns at once.
async fn sync(home: &Path) -> anyhow::Result<serde_json::Value> {
    identity::sync(home, &Config::new("Lock test".into(), "tester".into())).await
}

#[tokio::test]
async fn a_running_update_is_refused_and_a_starting_child_is_waited_out() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path();
    std::fs::create_dir_all(home.join("secrets")).unwrap();
    std::fs::write(home.join("secrets/portrait-disabled"), b"disabled").unwrap();

    // An update that is really running holds the lock throughout. A second one
    // is refused, with the same words as before, after a bounded wait.
    let running = identity::lock_retrying(home, []).await.unwrap();
    let started = Instant::now();
    assert_eq!(sync(home).await.unwrap_err().to_string(), BUSY);
    assert!(started.elapsed() < Duration::from_secs(5));
    drop(running);
    assert_eq!(sync(home).await.unwrap()["state"], "not_connected");

    #[cfg(unix)]
    {
        use std::{
            io::{Read, Write},
            os::{fd::AsRawFd, unix::process::CommandExt},
        };
        let (mut forked, announce) = std::io::pipe().unwrap();
        let (wait, mut release) = std::io::pipe().unwrap();
        // The lock is open here, as during an update, while another part of
        // the process starts a child.
        let held = identity::lock_retrying(home, []).await.unwrap();
        let (announce_fd, wait_fd) = (announce.as_raw_fd(), wait.as_raw_fd());
        let child = std::thread::spawn(move || {
            let mut command = std::process::Command::new("true");
            // Runs in the child between fork and exec. It says that it exists
            // and stays there until released, holding its copy of every
            // descriptor that was open at the fork, the lock included.
            unsafe {
                command.pre_exec(move || {
                    let mut byte = [0u8; 1];
                    libc::write(announce_fd, byte.as_ptr().cast(), 1);
                    libc::read(wait_fd, byte.as_mut_ptr().cast(), 1);
                    Ok(())
                });
            }
            command.status().unwrap()
        });
        forked.read_exact(&mut [0u8; 1]).unwrap();
        // The update ends. Nothing in this process holds the lock now; only the
        // child's copy of its descriptor does.
        drop(held);
        // A single attempt, as the connector made before, is refused although
        // no update is running.
        let single = identity::lock_retrying(home, []).await;
        assert_eq!(single.unwrap_err().to_string(), BUSY);
        // Retried: after the first busy attempt the child goes on to exec,
        // which closes its copy, and a later attempt takes the lock.
        let mut pauses = 0;
        let schedule = std::iter::from_fn(|| {
            if pauses == 0 {
                release.write_all(b"x").unwrap();
            }
            pauses += 1;
            (pauses <= 400).then_some(Duration::from_millis(25))
        });
        let taken = identity::lock_retrying(home, schedule).await.unwrap();
        assert!(pauses >= 1);
        assert!(child.join().unwrap().success());
        drop(taken);
        assert_eq!(sync(home).await.unwrap()["state"], "not_connected");
    }
}
