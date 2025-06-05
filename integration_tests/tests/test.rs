use std::{net::Shutdown, os::fd::AsRawFd, path::PathBuf, process::Stdio, time::Duration};

use nix::fcntl::{fcntl, FcntlArg, FdFlag};
use regex::Regex;
use serial_test::serial;
use tokio::{
    io::AsyncReadExt,
    net::{UnixDatagram, UnixListener},
    signal::unix::SignalKind,
    task::JoinHandle,
};

fn unset_cloexec(fd: i32) {
    let flags = fcntl(fd, FcntlArg::F_GETFD).expect("Failed to get info for file descriptor");
    let mut flags = FdFlag::from_bits(flags).unwrap();
    flags.remove(FdFlag::FD_CLOEXEC);
    let _ = fcntl(fd, FcntlArg::F_SETFD(flags)).expect("Unset failed!");
}

struct MockSystemdSocket(UnixDatagram, String);

impl MockSystemdSocket {
    fn new(path: &str) -> Self {
        let socket = UnixDatagram::bind(path).unwrap();
        Self(socket, path.to_owned())
    }

    async fn cleanup(self) {
        let (socket, path) = (self.0, self.1);
        socket.shutdown(Shutdown::Both).unwrap();
        tokio::fs::remove_file(path).await.unwrap();
    }

    async fn receive_string(&self, timeout: Duration) -> String {
        let mut recv_buf = vec![0_u8; 65_507];
        let result = tokio::time::timeout(timeout, self.0.recv_from(&mut recv_buf))
            .await
            .unwrap();
        match result {
            Ok((read_cnt, _addr)) => String::from_utf8(recv_buf[..read_cnt].to_vec()).unwrap(),
            Err(e) => panic!("{e}"),
        }
    }

    async fn expect_datagram(&self, expected_msg_pattern: &str, timeout: Duration) {
        let expected_re = Regex::new(expected_msg_pattern).unwrap();
        let received_msg = self.receive_string(timeout).await;
        if !expected_re.is_match(received_msg.as_str()) {
            panic!(
                "received message did not match expected pattern:
                            expected: {:?}
                            received: {:?}",
                expected_msg_pattern, received_msg
            )
        }
    }

    async fn expect_ready_datagram(&self, timeout: Duration) -> u32 {
        let expected_msg_pattern = r"^READY=1\r?\nMAINPID=(?<mainpid>[0-9]+)$";
        let expected_re = Regex::new(expected_msg_pattern).unwrap();
        let received_msg = self.receive_string(timeout).await;

        if let Some(captures) = expected_re.captures(&received_msg) {
            if let Some(main_pid_match) = captures.name("mainpid") {
                return main_pid_match.as_str().parse::<u32>().unwrap();
            }
        }
        panic!("Did not receive message with MAINPID!");
    }

    async fn expect_stopping_datagram(&self, timeout: Duration) {
        self.expect_datagram(r"^STOPPING=1$", timeout).await;
    }

    async fn expect_reloading_datagrams(&self, timeout: Duration) -> u32 {
        self.expect_datagram(r"^RELOADING=1(\r?\n)MONOTONIC_USEC=([0-9]+)$", timeout)
            .await;
        self.expect_ready_datagram(timeout).await
    }
}

struct SystemdActivatedUnixSocket {
    name: String,
    sock: UnixListener,
    path: PathBuf,
}

impl SystemdActivatedUnixSocket {
    async fn new() -> Self {
        let path = PathBuf::from("/tmp/ecdysis_int_test.sock");
        let _ = tokio::fs::remove_file(&path).await; //cleanup any older socket path

        let sock = UnixListener::bind(&path).unwrap();
        unset_cloexec(sock.as_raw_fd()); // pass this fd to the child
        SystemdActivatedUnixSocket {
            name: "ecdysis_test_unix".to_string(),
            sock,
            path,
        }
    }

    fn name_fd_tuple(&self) -> (String, i32) {
        (self.name.clone(), self.sock.as_raw_fd())
    }
}

fn build_listen_fdnames_env((name, fd): (String, i32)) -> String {
    const FD_START: i32 = 3;
    assert!(fd >= 3);

    let mut names = vec!["unknown"; (fd - FD_START) as usize];
    names.push(&name);
    names.join(":")
}

async fn spawn_ecdysis_echo_server(
    systemd_notify_socket_path: String,
    systemd_sock: (String, i32),
) -> JoinHandle<()> {
    let listen_fdnames = build_listen_fdnames_env(systemd_sock);

    let handle = tokio::task::spawn(async move {
        let mut command = tokio::process::Command::new("cargo");
        command
            .args([
                "run",
                "--example",
                "tokio_echo",
                "--features",
                "systemd",
                "--release",
            ])
            .envs([
                ("RUST_BACKTRACE", "full".into()),
                ("NOTIFY_SOCKET", systemd_notify_socket_path),
                ("LISTEN_FDNAMES", listen_fdnames),
            ])
            .current_dir("../")
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        let mut spawned_child = command.spawn().unwrap();
        // give the process some time to start
        tokio::time::sleep(Duration::from_secs(5)).await;

        let exit_status = spawned_child.wait().await.unwrap();
        assert!(exit_status.success());
    });
    handle
}

async fn is_process_running(pid: u32) -> bool {
    // kill -0 checks whether the current user can send signals to a specific PID.
    // It can therefore also be used to check whether a specific PID is running!
    // https://unix.stackexchange.com/questions/169898/what-does-kill-0-do
    let mut cmd = tokio::process::Command::new("kill");
    cmd.arg("-0").arg(format!("{pid}"));
    let mut proc = cmd.spawn().unwrap();
    let status = proc.wait().await.unwrap();
    status.success() // kill will fail if it can't send the signal to the process
}

async fn wait_for_process_shutdown(pid: u32, timeout: Duration) {
    let running_check_fut = async move {
        while is_process_running(pid).await {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    };
    tokio::time::timeout(timeout, running_check_fut)
        .await
        .unwrap();
}

async fn signal_process(pid: u32, signal_kind: SignalKind) {
    println!(
        "Sending signal {} to PID {}",
        signal_kind.as_raw_value(),
        pid
    );
    let mut cmd = tokio::process::Command::new("kill");
    cmd.arg(format!("-{}", signal_kind.as_raw_value()))
        .arg(format!("{pid}"));
    let mut proc = cmd.spawn().unwrap();
    let status = proc.wait().await.unwrap();
    assert!(status.success())
}

async fn assert_reload_count(address: &str, count: u32) {
    let mut client_stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let reload_count = client_stream.read_u32().await.unwrap();
    assert_eq!(reload_count, count);
}

async fn assert_reload_count_unix(path: &str, count: u32) {
    let mut client_stream = tokio::net::UnixStream::connect(path).await.unwrap();
    let reload_count = client_stream.read_u32().await.unwrap();
    assert_eq!(reload_count, count);
}

#[tokio::test]
#[serial]
async fn test_signal_reload_and_full_shutdown() {
    let systemd_notify_socket_path = "/var/run/ecdysis.sock";

    let mock_systemd_socket = MockSystemdSocket::new(systemd_notify_socket_path);
    let systemd_activated_sock = SystemdActivatedUnixSocket::new().await;
    let handle = spawn_ecdysis_echo_server(
        systemd_notify_socket_path.to_owned(),
        systemd_activated_sock.name_fd_tuple(),
    )
    .await;

    // wait for the systemd ready notification
    let parent_pid = mock_systemd_socket
        .expect_ready_datagram(Duration::from_secs(10))
        .await;
    assert!(is_process_running(parent_pid).await);

    assert_reload_count("localhost:22222", 0).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 0).await;

    // reload the process
    signal_process(parent_pid, SignalKind::hangup()).await;
    let child_pid = mock_systemd_socket
        .expect_reloading_datagrams(Duration::from_secs(10))
        .await;
    assert!(is_process_running(child_pid).await);
    assert_ne!(child_pid, parent_pid);
    wait_for_process_shutdown(parent_pid, Duration::from_secs(10)).await;
    assert_reload_count("localhost:22222", 1).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 1).await;

    // shut the process down
    signal_process(child_pid, SignalKind::user_defined1()).await;
    mock_systemd_socket
        .expect_stopping_datagram(Duration::from_secs(10))
        .await;

    // wait for process to die
    wait_for_process_shutdown(child_pid, Duration::from_secs(10)).await;

    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .unwrap()
        .unwrap();
    mock_systemd_socket.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_signal_partial_shutdown() {
    let systemd_notify_socket_path = "/var/run/ecdysis.sock";

    let mock_systemd_socket = MockSystemdSocket::new(systemd_notify_socket_path);
    let systemd_activated_sock = SystemdActivatedUnixSocket::new().await;
    let handle = spawn_ecdysis_echo_server(
        systemd_notify_socket_path.to_owned(),
        systemd_activated_sock.name_fd_tuple(),
    )
    .await;

    // wait for the systemd ready notification
    let parent_pid = mock_systemd_socket
        .expect_ready_datagram(Duration::from_secs(10))
        .await;
    assert!(is_process_running(parent_pid).await);

    assert_reload_count("localhost:22222", 0).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 0).await;

    // initiate a partial shutdown
    signal_process(parent_pid, SignalKind::user_defined2()).await;

    // in a partial shutdown, we should _not_ receive a STOPPING notification
    let timeout_fut = tokio::time::sleep(Duration::from_secs(10));
    tokio::select! {
        _ = mock_systemd_socket.expect_stopping_datagram(Duration::from_secs(30)) => {
            panic!("received unexpected stop notification");
        },
        _ = timeout_fut => panic!("timed out!"),
        _ = async {
            wait_for_process_shutdown(parent_pid, Duration::from_secs(10)).await;
            handle.await.unwrap();
        } => ()
    }
    mock_systemd_socket.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_socket_reload_and_shutdown() {
    let systemd_notify_socket_path = "/var/run/ecdysis.sock";

    let mock_systemd_socket = MockSystemdSocket::new(systemd_notify_socket_path);
    let systemd_activated_sock = SystemdActivatedUnixSocket::new().await;
    let handle = spawn_ecdysis_echo_server(
        systemd_notify_socket_path.to_owned(),
        systemd_activated_sock.name_fd_tuple(),
    )
    .await;

    // wait for the systemd ready notification
    let parent_pid = mock_systemd_socket
        .expect_ready_datagram(Duration::from_secs(10))
        .await;
    assert!(is_process_running(parent_pid).await);

    assert_reload_count("localhost:22222", 0).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 0).await;

    // reload the process using the socket api
    tokio::task::spawn(async {
        tokio::net::UnixStream::connect("/tmp/ecdysis_upgrade.sock")
            .await
            .unwrap();
    });
    let child_pid = mock_systemd_socket
        .expect_reloading_datagrams(Duration::from_secs(10))
        .await;
    assert!(is_process_running(child_pid).await);
    assert_ne!(child_pid, parent_pid);
    wait_for_process_shutdown(parent_pid, Duration::from_secs(10)).await;
    assert_reload_count("localhost:22222", 1).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 1).await;

    // shut the process down using the socket api
    tokio::task::spawn(async {
        tokio::net::UnixStream::connect("/tmp/ecdysis_exit.sock")
            .await
            .unwrap();
    });
    mock_systemd_socket
        .expect_stopping_datagram(Duration::from_secs(10))
        .await;

    // wait for processes to die
    wait_for_process_shutdown(child_pid, Duration::from_secs(10)).await;

    tokio::time::timeout(Duration::from_secs(10), handle)
        .await
        .unwrap()
        .unwrap();
    mock_systemd_socket.cleanup().await;
}

#[tokio::test]
#[serial]
async fn test_socket_partial_shutdown() {
    let systemd_notify_socket_path = "/var/run/ecdysis.sock";

    let mock_systemd_socket = MockSystemdSocket::new(systemd_notify_socket_path);
    let systemd_activated_sock = SystemdActivatedUnixSocket::new().await;
    let handle = spawn_ecdysis_echo_server(
        systemd_notify_socket_path.to_owned(),
        systemd_activated_sock.name_fd_tuple(),
    )
    .await;

    // wait for the systemd ready notification
    let parent_pid = mock_systemd_socket
        .expect_ready_datagram(Duration::from_secs(10))
        .await;
    assert!(is_process_running(parent_pid).await);

    assert_reload_count("localhost:22222", 0).await;
    assert_reload_count_unix(systemd_activated_sock.path.to_str().unwrap(), 0).await;

    // partial shutdown using the socket api
    tokio::task::spawn(async {
        tokio::net::UnixStream::connect("/tmp/ecdysis_partial_exit.sock")
            .await
            .unwrap();
    });

    // in a partial shutdown, we should _not_ receive a STOPPING notification
    let timeout_fut = tokio::time::sleep(Duration::from_secs(10));
    tokio::select! {
        _ = mock_systemd_socket.expect_stopping_datagram(Duration::from_secs(30)) => {
            panic!("received unexpected stop notification");
        },
        _ = timeout_fut => panic!("timed out!"),
        _ = async {
            wait_for_process_shutdown(parent_pid, Duration::from_secs(10)).await;
            handle.await.unwrap();
        } => ()
    }
    mock_systemd_socket.cleanup().await;
}
