use std::{
    env,
    io::{Error, Read},
    os::unix::{io::AsRawFd, process::CommandExt},
    process::{Child, Command},
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use bincode::serialize_into;

use crate::{
    registry::ListenerInfo,
    utils::{
        clone_fd, close_fd_quiet, unset_cloexec, ENV_PIPE_FDS, ENV_PIPE_READY, ENV_UPGRADE,
        UPGRADE_TRUE_VAL,
    },
};

pub type UpgradeFinished = Result<(), UpgradeError>;

#[derive(derive_more::From, derive_more::Display)]
#[display("{_variant}")]
pub enum UpgradeError {
    #[display("child exited unexpectedly")]
    ChildExit,

    #[display("timed out waiting for ready signal from child")]
    ChildTimeout,

    #[display("upgrade not started: {}", _0)]
    NotStarted(String),

    #[display("serialization error: {:?}", _0)]
    #[from]
    SerializationError(bincode::Error), //TODO: grr, figure out bincode error
}

pub fn upgrade(fds: Vec<ListenerInfo>) -> UpgradeFinished {
    // Equivalent to .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed).is_err()
    if UPGRADING.swap(true, Ordering::Acquire) {
        return Err(UpgradeError::NotStarted(String::from("Already in upgrade")));
    }

    log::debug!("In child, inherited files should be:\n {:?}", fds);
    let pipes = UpgradePipes::new()?;
    let child = exec_upgraded(&pipes.fds, fds.clone())?;
    let (recv_ready, send_listeners) = pipes.take_pipes();

    let send = send_fds(send_listeners, fds);
    let waitc = wait_child(child);
    let waitr = wait_ready(recv_ready);

    // The waitr thread is the arbiter of "moving on". It will
    // end when the child exits (with an error), when threads can't spawn, or
    // when the child successfully declares ready.
    let mut res = match waitr.join() {
        Ok(r) => r,
        _ => Err(UpgradeError::ChildExit),
    };

    UPGRADING.store(false, Ordering::Release);

    // Now we've cancelled timeout, so wait for that:
    waitc.thread().unpark();
    match waitc.join() {
        Ok(Ok(())) => (), // child still running
        Ok(Err(e)) => {
            res = Err(e); // child exited or a timeout happened, this gives us which
                          // even if the child declared ready, overwrite that, because the child
                          // exited if this arm is running.
        }
        Err(_) => panic!("Thread error in upgrade!"),
    }

    // this won't tell us anything new
    let _ = send.join();
    res
}

// This has two uses:
// 1. Prevent double upgrades, as this will not be set back to false until after the upgrade
//    process is complete (on success or failure of the upgrade)
// 2. Cancel the wait_child thread; once the upgrade is finished, wait() is not what we want to do.
pub(crate) static UPGRADING: AtomicBool = AtomicBool::new(false);

impl From<Error> for UpgradeError {
    fn from(e: Error) -> UpgradeError {
        UpgradeError::NotStarted(format!("{:?}", e))
    }
}

// Helper structs for managing all the pipe ends.

// FdPair holds the raw file descriptors - this is a separate struct because we need to
// impl Drop to make sure the fdesc dups are properly closed
struct FdPair {
    recv_listeners_fd: i32,
    send_ready_fd: i32,
}

impl Drop for FdPair {
    fn drop(&mut self) {
        // by the time UpgradePipes that holds this is dropped, the child will have spwaned, and
        // they are no longer neaded. This stops an fd leak.
        close_fd_quiet(self.recv_listeners_fd);
        close_fd_quiet(self.send_ready_fd);
    }
}

// UpgradePipes keeps track of all the pipe ends, allowing us to let the unused ends drop as
// soon as possible, even across the fork/exec. The drops will automatically cleanup the resources
// (see also FdPair, for how this is handled with raw fds).
struct UpgradePipes {
    // for parent
    recv_ready: os_pipe::PipeReader,
    send_listeners: os_pipe::PipeWriter,

    // for child
    fds: FdPair,
}

impl UpgradePipes {
    // The use of separate functions allows us to drop and close the unused os_pipe::Pipe* end as
    // soon as possible, to prevent fd leaks
    fn new() -> Result<UpgradePipes, UpgradeError> {
        let (recv_listeners_fd, send_listeners) = listener_pipes()?;
        let (recv_ready, send_ready_fd) = ready_pipes().inspect_err(|_| {
            close_fd_quiet(recv_listeners_fd);
        })?;

        let fds = FdPair {
            recv_listeners_fd,
            send_ready_fd,
        };

        Ok(Self {
            recv_ready,
            send_listeners,
            fds,
        })
    }

    // make dropping happy
    fn take_pipes(self) -> (os_pipe::PipeReader, os_pipe::PipeWriter) {
        (self.recv_ready, self.send_listeners)
    }
}

// Worker threads
// Send the ListenerInfos to the child.
fn send_fds(
    send_pipe: os_pipe::PipeWriter,
    fds: Vec<ListenerInfo>,
) -> thread::JoinHandle<UpgradeFinished> {
    thread::spawn(move || -> UpgradeFinished {
        serialize_into(send_pipe, &fds).map_err(|e| e.into())
    })
}

// Check that the child is still running for 5 seconds after launch. If the child
// exits or the there is a timeout, exit the thread. In the case of a timeout, kill the child
// before exiting. Thread exits successfully if neither of those conditions have been met when
// the spawning thread sets UPGRADING to false.
//
// TODO: timeout param
fn wait_child(mut child: Child) -> thread::JoinHandle<UpgradeFinished> {
    thread::spawn(move || {
        let start = Instant::now();
        let timeout = Duration::from_secs(5);

        while start.elapsed() < timeout {
            thread::sleep(Duration::from_millis(500));

            proc_wait(&mut child)?;

            if !UPGRADING.load(Ordering::Acquire) {
                return proc_wait(&mut child);
            }
        }

        let _ = child.kill();
        // wait again to reap
        let _ = child.wait();
        Err(UpgradeError::ChildTimeout)
    })
}

fn proc_wait(child: &mut Child) -> UpgradeFinished {
    match child.try_wait() {
        Ok(None) => Ok(()),
        _ => Err(UpgradeError::ChildExit),
    }
}

// Wait for the child to declare itself ready.
fn wait_ready(mut recv_ready: os_pipe::PipeReader) -> thread::JoinHandle<UpgradeFinished> {
    thread::spawn(move || -> UpgradeFinished {
        let mut buf = [0; 2];

        if recv_ready.read_exact(&mut buf).is_ok() && &buf == b"OK" {
            return Ok(());
        }
        Err(UpgradeError::ChildExit)
    })
}

// Helpers
// Setup environment and launch the upgraded process
fn exec_upgraded(pipe_fds: &FdPair, inherit_fds: Vec<ListenerInfo>) -> Result<Child, Error> {
    let mut run_args: Vec<String> = env::args().collect();
    let cmdline = run_args.remove(0);
    let cwd = env::current_dir()?;

    let mut cmd = Command::new(cmdline);
    cmd.args(run_args)
        .current_dir(cwd)
        .env(ENV_UPGRADE, UPGRADE_TRUE_VAL)
        .env(ENV_PIPE_FDS, format!("{}", pipe_fds.recv_listeners_fd))
        .env(ENV_PIPE_READY, format!("{}", pipe_fds.send_ready_fd));

    #[cfg(feature = "systemd_sockets")]
    {
        // dont share LISTEN_* variables with the child process
        cmd.env_remove(crate::tokio_ecdysis::systemd_sockets::LISTEN_PID);
        cmd.env_remove(crate::tokio_ecdysis::systemd_sockets::LISTEN_FDNAMES);
        cmd.env_remove(crate::tokio_ecdysis::systemd_sockets::LISTEN_FDS);
    }

    // This will run after fork, and after setting up the child's stdin, stdout and stderr, but
    // before calling exec. We change the fds we will pass to the child to not have CLOEXEC bits
    // set. Since CLOEXEC is a property of the FD, not the underlying socket, and this occurs in a
    // different process, the FDs are not going to leak if the user forks for a different reason
    // than upgrade (e.g. a shell-out).
    unsafe {
        cmd.pre_exec(move || {
            for i in inherit_fds.iter() {
                unset_cloexec(i.fd);
            }
            Ok(())
        });
    }
    cmd.spawn()
}

// Create pipe for sending the ListenerInfos for open listeners to the child
fn listener_pipes() -> Result<(i32, os_pipe::PipeWriter), UpgradeError> {
    let (recv_listeners, send_listeners) = os_pipe::pipe()?;
    let recv_listeners_fd = clone_fd(recv_listeners.as_raw_fd())?;
    unset_cloexec(recv_listeners_fd);
    Ok((recv_listeners_fd, send_listeners))
}

// Create pipe for having the child notify the parent of success
fn ready_pipes() -> Result<(os_pipe::PipeReader, i32), UpgradeError> {
    let (recv_ready, send_ready) = os_pipe::pipe()?;
    let send_ready_fd = clone_fd(send_ready.as_raw_fd())?;
    unset_cloexec(send_ready_fd);
    Ok((recv_ready, send_ready_fd))
}
