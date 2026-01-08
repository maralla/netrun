use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::{fork, ForkResult, Pid};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

/// Spawn a child process. The closure runs in the child and returns a Command.
/// The child runs the returned `Command::exec()`; if that fails or the closure errors,
/// the child exits with code 1.
pub fn spawn<F>(child_fn: F) -> anyhow::Result<Pid>
where
    F: FnOnce() -> anyhow::Result<Command>,
{
    match unsafe { fork() }? {
        ForkResult::Parent { child } => Ok(child),
        ForkResult::Child => {
            match child_fn() {
                Ok(mut cmd) => {
                    let cmd_repr = format!("{cmd:?}");
                    let err = cmd.exec();
                    eprintln!("Fail to run command {}, error: {}", cmd_repr, err);
                }
                Err(e) => {
                    eprintln!("Fail to run command setup, error: {}", e);
                }
            }
            std::process::exit(1);
        }
    }
}

/// Reap exited children, returns true if any remain
pub fn reap_children(pids: &mut Vec<Pid>) -> bool {
    loop {
        match waitpid(None, Some(WaitPidFlag::WNOHANG)) {
            Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                pids.retain(|&p| p != pid);
            }
            Ok(WaitStatus::StillAlive) => break,
            Err(nix::errno::Errno::ECHILD) => {
                pids.clear();
                break;
            }
            _ => break,
        }
    }
    !pids.is_empty()
}

/// Wait for children to exit, or until signal_count reaches threshold
fn wait_for_children(pids: &mut Vec<Pid>, signal_count: &Arc<AtomicU8>, threshold: u8) {
    while reap_children(pids) && signal_count.load(Ordering::Relaxed) < threshold {
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}

/// Manage children lifecycle: wait until signal, send SIGTERM, wait again, then SIGKILL
pub fn supervise_children(pids: &mut Vec<Pid>, signal_count: &Arc<AtomicU8>) {
    wait_for_children(pids, signal_count, 1);
    if pids.is_empty() {
        return;
    }

    for &pid in pids.iter() {
        let _ = kill(pid, Signal::SIGTERM);
    }

    wait_for_children(pids, signal_count, 2);

    if !pids.is_empty() {
        eprintln!("Force killing {} remaining process(es)", pids.len());
        for &pid in pids.iter() {
            let _ = kill(pid, Signal::SIGKILL);
            let _ = waitpid(pid, None);
        }
    }
}
