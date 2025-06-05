//! Implements a client for systemd-notify.
//! See the systemd-notify manpages for further details and explanations on message types:
//! - <https://www.freedesktop.org/software/systemd/man/latest/sd_notify.html#>
//! - <https://www.freedesktop.org/software/systemd/man/latest/systemd-notify.html>

use std::{
    convert::TryFrom,
    env,
    fmt::{self, Display, Formatter},
    io,
    time::Duration,
};

use nix::{
    time::{clock_gettime, ClockId},
    unistd::getpid,
};
use tokio::net::UnixDatagram;

#[derive(Debug)]
pub struct SystemdNotifier {
    socket: UnixDatagram,
}

#[derive(Debug)]
pub enum SystemdNotifierError {
    NotifySocketUnset,
    SocketCreateError(io::Error),
    SocketConnectError(io::Error),
}

impl Display for SystemdNotifierError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        match self {
            Self::NotifySocketUnset => {
                write!(f, "The NOTIFY_SOCKET environment variable is not set!")
            }
            Self::SocketCreateError(err) => {
                write!(
                    f,
                    "Error creating unbound unix datagram socket to receive systemd notifications: {err}"
                )
            }
            Self::SocketConnectError(err) => {
                write!(f, "Error connecting to NOTIFY_SOCKET: {err}")
            }
        }
    }
}

impl std::error::Error for SystemdNotifierError {}

impl SystemdNotifier {
    pub fn new() -> Result<Self, SystemdNotifierError> {
        let notify_sock_path =
            env::var("NOTIFY_SOCKET").map_err(|_| SystemdNotifierError::NotifySocketUnset)?;

        let socket =
            UnixDatagram::unbound().map_err(|err| SystemdNotifierError::SocketCreateError(err))?;

        // connect to the systemd socket to avoid having to constantly specify the address
        socket
            .connect(notify_sock_path)
            .map_err(|err| SystemdNotifierError::SocketConnectError(err))?;

        Ok(Self { socket })
    }

    pub async fn notify<'a, I, K, V>(&mut self, states: I) -> io::Result<()>
    where
        I: Iterator<Item = (K, V)>,
        K: AsRef<str> + 'a,
        V: AsRef<str> + 'a,
    {
        let message = states
            .map(|(k, v)| format!("{}={}", k.as_ref(), v.as_ref()))
            .collect::<Vec<String>>()
            .join("\n");
        let buf = message.as_bytes();
        self.socket.send(buf).await.map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("Failed to send notification to systemd: {e}"),
            )
        })?;
        Ok(())
    }

    pub async fn notify_ready(&mut self) -> io::Result<()> {
        // Note that there is no actual requirement to send MAINPID with every READY notification.
        // However, for the use-cases of ecdysis this makes sense, as it allows us to completely
        // get rid of pidfiles and is easier to implement than only sending MAINPID on reloads.
        let states = vec![("READY", "1".to_owned()), ("MAINPID", getpid().to_string())];
        self.notify(states.into_iter()).await
    }

    pub async fn notify_reloading(&mut self) -> io::Result<()> {
        // As per the manpage, when the reload begins we are supposed to send a monotonic timestamp
        // in microseconds, and this is a safe way of reading the system monotonic clock.
        let monotime_mus =
            std::time::Duration::from(clock_gettime(ClockId::CLOCK_MONOTONIC)?).as_micros();

        let monotime_mus = u64::try_from(monotime_mus).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "CLOCK_MONOTONIC value is greater than the size of a u64",
            )
        })?;

        let monotime_mus_str = format!("{monotime_mus}");

        let states = vec![("RELOADING", "1"), ("MONOTONIC_USEC", &monotime_mus_str)];
        self.notify(states.into_iter()).await
    }

    pub async fn notify_stopping(&mut self) -> io::Result<()> {
        let states = vec![("STOPPING", "1")];
        self.notify(states.into_iter()).await
    }

    pub async fn notify_extend_timeouts(&mut self, extension: Duration) -> io::Result<()> {
        let extension_mus = u64::try_from(extension.as_micros()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::Other,
                "extension value is greater than the size of a u64",
            )
        })?;
        let extension_mus_str = format!("{extension_mus}");

        let states = vec![("EXTEND_TIMEOUT_USEC", &extension_mus_str)];
        self.notify(states.into_iter()).await
    }
}
