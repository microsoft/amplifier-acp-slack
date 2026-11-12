#![cfg(windows)]
use acp_slack_adapter::process::{ProcessTree, prepare};
use std::{process::Stdio, time::Duration};
use tokio::process::Command;

fn alive(pid: u32) -> bool {
    use windows_sys::Win32::{Foundation::CloseHandle, System::Threading::*};
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
    if handle.is_null() {
        return false;
    }
    let running = unsafe { WaitForSingleObject(handle, 0) } == 0x102;
    unsafe {
        CloseHandle(handle);
    }
    running
}

#[tokio::test]
async fn child_cannot_run_before_job_and_immediate_descendant_stops_with_it() {
    let temp = tempfile::tempdir().unwrap();
    let folder = temp.path().join("café 子");
    std::fs::create_dir(&folder).unwrap();
    let marker = folder.join("child.json");
    let code = "import json,pathlib,subprocess,sys,time; p=subprocess.Popen([sys.executable,'-X','utf8','-c','import time; time.sleep(60)']); pathlib.Path(sys.argv[1]).write_text(json.dumps({'pid':p.pid}),encoding='utf-8'); time.sleep(60)";
    let mut command = Command::new("python");
    command
        .args(["-X", "utf8", "-c", code])
        .arg(&marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    prepare(&mut command);
    let mut child = command.spawn().unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(
        !marker.exists(),
        "Child executed before cleanup ownership was established"
    );
    let tree = ProcessTree::attach(&child).unwrap();
    let mut descendant = None;
    for _ in 0..100 {
        descendant = std::fs::read(&marker)
            .ok()
            .and_then(|v| serde_json::from_slice::<serde_json::Value>(&v).ok())
            .and_then(|v| v["pid"].as_u64())
            .map(|id| id as u32);
        if descendant.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let pid = descendant.expect("Child did not start after job attachment");
    assert!(alive(pid));
    drop(tree);
    tokio::time::timeout(Duration::from_secs(5), child.wait())
        .await
        .unwrap()
        .unwrap();
    for _ in 0..100 {
        if !alive(pid) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !alive(pid),
        "Immediate descendant survived its owned operation"
    );
}
