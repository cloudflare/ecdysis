use std::{env, os::unix::io::FromRawFd};

use bincode::deserialize_from;

use crate::{
    registry::ListenerInfo,
    utils::{ENV_PIPE_FDS, ENV_PIPE_READY, ENV_UPGRADE, UPGRADE_TRUE_VAL},
};

#[derive(derive_more::From, derive_more::Display)]
#[display("{_variant}")]
pub enum InheritError {
    NotAnUpgrade,

    #[display("failed to deserialize listeners received from parent: {:?}", _0)]
    #[from]
    DeserializationError(bincode::Error),

    #[display("invalid environment: {}", _0)]
    BadEnvironment(String),
}

pub fn init_child() -> Result<(Vec<ListenerInfo>, os_pipe::PipeWriter), InheritError> {
    // Are we in an upgrade?
    match env::var(ENV_UPGRADE) {
        Ok(ref s) if s == UPGRADE_TRUE_VAL => (),
        Err(env::VarError::NotPresent) => {
            log::info!("Initializing Ecdysis - upgrade: False");
            return Err(InheritError::NotAnUpgrade);
        }
        _ => {
            return Err(InheritError::BadEnvironment(format!(
                "Value of env var {} is not valid",
                ENV_UPGRADE
            )));
        }
    };
    log::info!("Initializing Ecdysis - upgrade: True");

    // Get the inherited listeners immediately.
    let inherit_fd = pipe_fd_from_env(ENV_PIPE_FDS)?;
    let inherit_pipe = unsafe { os_pipe::PipeReader::from_raw_fd(inherit_fd) };
    let inherited_listeners = deserialize_from(inherit_pipe)?;

    log::debug!("In child inherited files are:\n {:?}", inherited_listeners);
    // then get the ready notification pipe
    let ready_fd = pipe_fd_from_env(ENV_PIPE_READY)?;
    let ready_pipe = unsafe { os_pipe::PipeWriter::from_raw_fd(ready_fd) };

    Ok((inherited_listeners, ready_pipe))
}

fn pipe_fd_from_env(env_var: &str) -> Result<i32, InheritError> {
    let fd = env::var(env_var)
        .map_err(|_| InheritError::BadEnvironment(format!("cannot read env var {}", env_var)))?;
    fd.parse()
        .map_err(|_| InheritError::BadEnvironment(format!("{} is not an integer", env_var)))
}
