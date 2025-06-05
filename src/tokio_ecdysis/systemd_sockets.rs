//! Support for systemd activated sockets.
//!
//! The ecdysis library supports acquiring systemd activated sockets into the ecdysis registry.
//! Read [man systemd.socket](https://www.man7.org/linux/man-pages/man5/systemd.socket.5.html) for
//! background on systemd activated sockets. Then read
//! [man sd_listen_fds](https://www.man7.org/linux/man-pages/man3/sd_listen_fds.3.html) on how to
//! read the passed file descriptors.
//!
//! TLDR: systemd can create sockets on behalf of services and pass them to the service.
//! This is done in the following steps during fork-exec to start the new service:
//! 1. Before `exec` systemd creates the sockets and remove FD_CLOEXEC flag. This lets the exec-ed
//!    process inherit the socket file descriptors.
//! 3. Set environment variables LISTEN_* (described later) to provide information about the
//!    passed sockets.
//! 4. Exec into the new process.
//!
//! Then the new service starts and has open file descriptors corresponding to the sockets passed
//! by systemd.
//! The new service can read these environment variables to find the file descriptors:
//! 1. LISTEN_PID: This environment variable holds the process id of the process that systemd
//!    started. This is compared against the current process PID before reading the variables below.
//!    This ensures any child processes dont use the variables incorrectly.
//!    The ecdysis library will reset the variables before it spawns any child instances.
//!
//! 2. LISTEN_FDS: This environment variable holds the number of file descriptors passed to the
//!    process. If the value is N, the passed FDs are 3, 4,..., N+3. If its set to 0, no sockets will
//!    be passed to the process. The ecdysis library DOES NOT read LISTEN_FDS. See the next variable.
//!
//! 3. LISTEN_FD_NAMES: This environment variable holds names of the file descriptors that were
//!    passed to the process. The names are separated by a ":". These names can be used to then
//!    identify the sockets.
//!    e.g. if the value is "one_sock:second_sock:sock_3", then 3 sockets were passed:
//!    FD 3 is the one named "one_sock", FD 4 is the one named "second_sock", FD 5 is the one named
//!    "sock_3". This name corresponds to the name of from the systemd socket service file.
//!    This can be set by setting "FileDescriptorName=". See the systemd.socket man page for more
//!    information.
//!
//! Note:
//! 1. Ecdysis will reset all LISTEN_* environment variables before it spawns a child instance.
//! 2. Ecdysis will set the CLOEXEC flag for all sockets passed by systemd. This prevents socket
//!    leaks to child processes spawned by the process.
//! 3. Only sockets that the process finds by calling a "systemd_listen_unix"/"systemd_listen_tcp",
//!    etc. will be passed to the child spawned by ecdysis. Any sockets not found, will be lost.
//! 4. Ecdysis will ignore file descriptors if they have duplicate names. i.e. duplicate socket
//!    names in LISTEN_FDNAMES will be LOST.
//! 5. Ecdysis will ignore file descriptor with special names, documented in
//!    [man sd_listen_fds](https://www.man7.org/linux/man-pages/man3/sd_listen_fds.3.html)
//!    These special names are "unknown", "stored" and "connection".
//!
//!
use std::{
    collections::{HashMap, HashSet},
    env, io,
};

use tokio::sync::Mutex;

use crate::utils::set_cloexec;

pub(crate) struct SystemdSockets(Mutex<HashMap<String, i32>>);

pub(crate) const LISTEN_PID: &str = "LISTEN_PID";
pub(crate) const LISTEN_FDNAMES: &str = "LISTEN_FDNAMES";
pub(crate) const LISTEN_FDS: &str = "LISTEN_FDS";

// FDs start at 3
const SD_LISTEN_FDS_START: i32 = 3;

/// SystemdSocketsReadError is returned when systemd sockets are first read from environement
/// variables.
#[derive(Debug)]
pub enum SystemdSocketsReadError {
    /// missing or malformed env variables.
    EnvironmentVariableError(String),
    /// LISTEN_PID does not match the current PID.
    DoesNotMatchListenPid,
    /// [`crate::tokio_ecdysis::TokioEcdysis::read_systemd_sockets`] is called more than once.
    DuplicateSystemdSocketsRead,
}

/// SystemdSocketsReadError is returned when systemd sockets are read using the systemd_* calls
/// e.g. [`crate::tokio_ecdysis::TokioEcdysis::systemd_listen_unix`] to find a unix socket.
#[derive(Debug)]
pub enum SystemdSocketError {
    /// [`crate::tokio_ecdysis::TokioEcdysis::read_systemd_sockets`] was not called
    SystemdSocketsNotInitialized,
    /// The socket was not found among the sockets passed by systemd
    SocketNotFoundInParent(String),
    /// The socket was not found in the registry
    SocketNotFoundInChildRegistry(String),
    /// An IO Error
    IoError(io::Error),
    /// SockInfo does not match the socket's bind information
    SockInfoIncorrect(String),
}

impl From<io::Error> for SystemdSocketError {
    fn from(e: io::Error) -> Self {
        Self::IoError(e)
    }
}

impl SystemdSockets {
    /// new reads LISTEN_* environment variables to find file descriptors passed by systemd
    pub(crate) fn new() -> Result<SystemdSockets, SystemdSocketsReadError> {
        // Sockets described in
        // [man sd_listen_fds](https://www.man7.org/linux/man-pages/man3/sd_listen_fds.3.html)
        // under "Table 1.  Special names", are ignored while reading sockets from LISTEN_FDNAMES
        //
        // These names are being declared here because there isnt a easy way to declare a const
        // hashset at the top. This should be okay because `new` is only called once per instance.
        let special_socket_names: HashSet<&str> =
            HashSet::from(["unknown", "stored", "connection"]);

        let listen_pid_str: String = env::var(LISTEN_PID).map_err(|e| {
            SystemdSocketsReadError::EnvironmentVariableError(format!(
                "env LISTEN_PID missing {:?}",
                e
            ))
        })?;
        let listen_pid: i32 = listen_pid_str.parse::<i32>().map_err(|e| {
            SystemdSocketsReadError::EnvironmentVariableError(format!(
                "parsing pid from string failed {:?}",
                e
            ))
        })?;
        let listen_fd_names: String = env::var(LISTEN_FDNAMES).map_err(|e| {
            SystemdSocketsReadError::EnvironmentVariableError(format!(
                "env LISTEN_FDNAMES missing {:?}",
                e
            ))
        })?;

        log::debug!("LISTEN_PID: {}", listen_pid);
        log::debug!("LISTEN_FDNAMES: {}", listen_fd_names);

        // Ensure the PID matches (this is a safety check)
        if listen_pid != std::process::id() as i32 {
            return Err(SystemdSocketsReadError::DoesNotMatchListenPid);
        }

        let names: Vec<&str> = listen_fd_names.split(':').collect();
        let mut map: HashMap<String, i32> = HashMap::new();

        for (i, &fd_name) in names.iter().enumerate() {
            let fd = SD_LISTEN_FDS_START + i as i32;
            set_cloexec(fd);
            if special_socket_names.contains(fd_name) {
                log::warn!(
                    "socket name not supported. FDNAME \"{}\", fd {} ignored",
                    fd_name,
                    fd
                );
            } else if map.contains_key(fd_name) {
                log::warn!("duplicate FDNAME \"{}\", fd {} ignored", fd_name, fd);
                // this fd has cloexec set. It will not be closed immediately, but will not be
                // passed to any children
            } else {
                map.insert(fd_name.to_string(), fd);
            }
        }

        Ok(SystemdSockets(Mutex::new(map)))
    }

    // find a fd corresponding to a name. The caller has to add it to the registry,to pass the
    // sockets to the child
    pub(crate) async fn find(&self, name: String) -> Result<i32, SystemdSocketError> {
        let locked_map = self.0.lock().await;

        match locked_map.get(&name) {
            Some(&fd) => Ok(fd),
            None => Err(SystemdSocketError::SocketNotFoundInParent(format!(
                "fd {:?} not found LISTEN_FDNAMES",
                name
            ))),
        }
    }
}
