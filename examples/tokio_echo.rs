use std::{
    env, io,
    net::{IpAddr, SocketAddr},
    str::FromStr,
    time::{Duration, Instant},
};

use futures::{Stream, StreamExt};
use nix::unistd::getpid;
use tokio::{
    io::{copy, split, AsyncRead, AsyncWrite, AsyncWriteExt},
    net::{TcpStream, UnixStream},
};

use ecdysis::tokio_ecdysis::{SignalKind, StopOnShutdown, TokioEcdysisBuilder};

// Exit the process after this Duration, even if tasks remain.
const EXIT_AFTER_UPGRADE_DURATION: Duration = std::time::Duration::from_secs(60);

pub trait AsyncReadWrite: AsyncRead + AsyncWrite {}
impl AsyncReadWrite for TcpStream {}
impl AsyncReadWrite for UnixStream {}

async fn echo_server<S, C>(mut sock_stream: S)
where
    S: Stream<Item = io::Result<C>> + Unpin + Sized,
    C: AsyncReadWrite + Unpin + Send + 'static,
{
    // Whenever a new connection is established, the first message sent by this echo server
    // corresponds to a single uint32 indicating the number of reloads so far.

    let reload_count = env::var("ECDYSIS_RELOADS").unwrap().parse::<u32>().unwrap();

    // TODO: Consider using "Structured Concurrency" when that lands:
    // https://github.com/tokio-rs/tokio/issues/1879
    let wg = waitgroup::WaitGroup::new();

    while let Some(Ok(mut client)) = sock_stream.next().await {
        let w = wg.worker();
        let client_fut = async move {
            client.write_all(&reload_count.to_be_bytes()).await.unwrap();
            let (mut client_r, mut client_w) = split(client);
            let _n_bytes = copy(&mut client_r, &mut client_w).await;

            drop(w); // Task finished
        };
        tokio::spawn(client_fut);
    }

    wg.wait().await;
}

// single thread needed for set_listen_pid
#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init_from_env(
        env_logger::Env::default().filter_or(env_logger::DEFAULT_FILTER_ENV, "info"),
    );
    set_listen_pid();

    let reload_count = match env::var("ECDYSIS_RELOADS") {
        Err(_) => {
            env::set_var("ECDYSIS_RELOADS", "0");
            0
        }
        Ok(s) => {
            let reload_count = s.parse::<u32>().unwrap() + 1;
            env::set_var("ECDYSIS_RELOADS", format!("{reload_count}").as_str());
            reload_count
        }
    };

    let pid = getpid();
    if reload_count == 0 {
        log::info!("echo server parent started (PID: {pid})");
    } else {
        log::info!("echo server child started (PID: {pid}; reload count: {reload_count})");
    }

    let mut ecdysis_builder = TokioEcdysisBuilder::new(SignalKind::hangup()).unwrap();
    ecdysis_builder
        .stop_on_signal(SignalKind::user_defined1())
        .unwrap();
    ecdysis_builder
        .partial_stop_on_signal(SignalKind::user_defined2())
        .unwrap();
    ecdysis_builder
        .upgrade_on_socket("/tmp/ecdysis_upgrade.sock")
        .unwrap();
    ecdysis_builder
        .stop_on_socket("/tmp/ecdysis_exit.sock")
        .unwrap();
    ecdysis_builder
        .partial_stop_on_socket("/tmp/ecdysis_partial_exit.sock")
        .unwrap();
    ecdysis_builder.set_pid_file("./pidfile");
    #[cfg(feature = "systemd_notify")]
    if let Err(err) = ecdysis_builder.enable_systemd_notifications() {
        log::info!("Failed to enable systemd notifications: {err}");
    }

    #[cfg(feature = "systemd_sockets")]
    if let Err(err) = ecdysis_builder.read_systemd_sockets() {
        log::error!("Failed to read systemd sockets: {err:?}");
    }

    let ip_addr = match IpAddr::from_str("[::1]") {
        Ok(ip_addr) => ip_addr,
        Err(_) => IpAddr::from_str("0.0.0.0").unwrap(),
    };
    let addr = SocketAddr::new(ip_addr, 22222);

    log::info!("Address is: {:?}", addr);

    let stream = ecdysis_builder
        .build_listen_tcp(StopOnShutdown::Yes, addr, |b, addr| {
            if ip_addr.is_ipv6() {
                b.set_only_v6(true).expect("cannot set v6 here");
            }
            b.set_reuse_address(true).expect("Cannot set REUSEADDR");
            b.bind(&addr.into()).expect("Cannot bind to provided IP");
            b.listen(128)?;
            Ok(b.into())
        })
        .unwrap();
    let server_handle = tokio::spawn(echo_server(stream));

    #[cfg(feature = "systemd_sockets")]
    let systemd_server_handle = {
        let sd_unix_stream = ecdysis_builder
            .systemd_listen_unix(
                StopOnShutdown::Yes,
                "ecdysis_test_unix".to_string(),
                "/tmp/ecdysis_int_test.sock".to_string(),
            )
            .await
            .unwrap();
        tokio::spawn(echo_server(sd_unix_stream))
    };

    let (_tokio_ecdysis, ecdysis_fut) = ecdysis_builder.ready().unwrap();

    let exit = ecdysis_fut.await;
    log::info!("Shutdown because: {:?}", exit);
    let exit_start_time = Instant::now();

    // Service existing conections for up to 1 minute
    tokio::spawn(async {
        tokio::time::sleep(EXIT_AFTER_UPGRADE_DURATION).await;
        log::info!(
            "Force-exiting {:?} after upgrade",
            EXIT_AFTER_UPGRADE_DURATION
        );
        std::process::exit(1)
    });

    // add a forceful 1 second sleep here to be able to properly test synchronous shutdowns
    tokio::time::sleep(Duration::from_secs(1)).await;

    server_handle.await.unwrap();
    #[cfg(feature = "systemd_sockets")]
    systemd_server_handle.await.unwrap();
    log::info!(
        "Graceful exit {:?} after ecdysis stop (reason: {exit:?})",
        exit_start_time.elapsed()
    );
}

fn set_listen_pid() {
    // ideally systemd should set LISTEN_PID and LISTEN_FDNAMES to read systemd activated sockets.
    // but for our tests, LISTEN_PID cant be set because there is not easy way to set the pid
    // before the test process is exec-ed.
    // So this is a hack, where LISTEN_PID will be set if LISTEN_FDNAMES is set.
    //
    // set_var is undefined in multithreaded environment. So echo_server uses single thread environment.

    match env::var("LISTEN_FDNAMES") {
        Ok(v) => {
            if v.is_empty() {
                return;
            }
        }
        Err(_) => return,
    }

    // a non empty LISTEN_FDNAMES value was set, so write LISTEN_PID too
    let pid = format!("{}", std::process::id());
    env::set_var("LISTEN_PID", pid);
    log::info!("LISTEN_PID updated");
}
