use std::{io, path::Path};

use nix::{
    fcntl::{fcntl, FcntlArg, FdFlag},
    unistd,
};

pub(crate) const ENV_UPGRADE: &str = "ECDYSIS_RUST_UPGRADE";
pub(crate) const ENV_PIPE_READY: &str = "ECDYSIS_RUST_PIPE_READY";
pub(crate) const ENV_PIPE_FDS: &str = "ECDYSIS_RUST_PIPE_FDS";
pub(crate) const UPGRADE_TRUE_VAL: &str = "yes";

// TODO: Don't die here
pub(crate) fn set_cloexec(fd: i32) {
    let flags = fcntl(fd, FcntlArg::F_GETFD).expect("Failed to get info for file descriptor");
    let flags = FdFlag::from_bits(flags).expect("unknown fd flags") | FdFlag::FD_CLOEXEC;
    let _ = fcntl(fd, FcntlArg::F_SETFD(flags));
}

pub(crate) fn unset_cloexec(fd: i32) {
    let flags = fcntl(fd, FcntlArg::F_GETFD).expect("Failed to get info for file descriptor");
    let mut flags = FdFlag::from_bits(flags).unwrap();
    flags.remove(FdFlag::FD_CLOEXEC);
    let _ = fcntl(fd, FcntlArg::F_SETFD(flags)).expect("Unset failed!");
}

pub(crate) fn clone_fd(to_clone: i32) -> io::Result<i32> {
    fcntl(to_clone, FcntlArg::F_DUPFD_CLOEXEC(to_clone))
        .map_err(|errno| io::Error::from_raw_os_error(errno as i32))
}

//TODO: die here?
pub(crate) fn close_fd_quiet(fd: i32) {
    let _ = unistd::close(fd);
}

// Panic here is because an error in writng the pidfile can result in unknown behaviors in external
// programs that need the pidfile to be correct
pub(crate) fn write_pid_file<P: AsRef<Path>>(path: P) -> io::Result<()> {
    use std::io::Write;
    // current proc pid
    let pid = unistd::getpid().as_raw();

    // make the tempfile in the same dir a the pidfile - the tempfile crate doesn't do atomic swaps
    // across filesystems, so this ensures that the tmpfile is in the same fs as the actual pidfile
    let dir: &Path = path.as_ref().parent().expect("invalid path to pidfile");

    let tmp_pidfile = tempfile::Builder::new()
        .tempfile_in(dir)
        .expect("cannot create pidfile!");

    // Actually store the current pid
    tmp_pidfile
        .as_file()
        .write_all(format!("{}", pid).as_bytes())
        .expect("cannot write pidfile!");

    // add pidfile (or replace existing one) atomically
    tmp_pidfile.persist(path).expect("cannot write pidfile!");

    Ok(())
}
