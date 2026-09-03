//! Transactional transfer of application-owned file descriptors and state between generations.
//!
//! Ecdysis can preserve listening sockets without application involvement. Active connections are
//! different: the application must stop using them and preserve any userspace protocol state before
//! they can move. This module supplies the transport and transaction boundary while leaving those
//! protocol-specific steps to the application.
//!
//! Before sending an item, the parent must quiesce it without irreversibly discarding state. It
//! keeps ownership of its original descriptors until [`PreparedHandover::commit`] succeeds. The
//! child may reconstruct state from received descriptors, but must leave it dormant until
//! [`PreparedIncoming::wait_for_commit`] returns. Either side may abort before the child declares
//! itself prepared; after that declaration, activation must be infallible, while the parent may
//! still abort until it commits. A child using [`crate::Ecdysis`] should pass the returned
//! [`HandoverCommit`] to [`crate::Ecdysis::complete_handover`] before `ready`.

use std::{
    fmt,
    io::{self, IoSlice, IoSliceMut},
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
    os::unix::net::UnixDatagram,
    time::Duration,
};

use nix::sys::socket::{recvmsg, sendmsg, ControlMessage, ControlMessageOwned, MsgFlags, UnixAddr};

use crate::utils::set_cloexec;

const MAGIC: &[u8; 4] = b"ECDH";
const WIRE_VERSION: u8 = 1;
const HEADER_LEN: usize = 24;
const HARD_MAX_PAYLOAD_SIZE: usize = 64 * 1024;
const HARD_MAX_FDS_PER_ITEM: usize = 64;
const HARD_MAX_NAME_SIZE: usize = 255;
const HARD_MAX_ITEMS: usize = 4096;
const HARD_MAX_TOTAL_PAYLOAD_SIZE: usize = 16 * 1024 * 1024;
const HARD_MAX_TOTAL_FDS: usize = 4096;

/// Default upper bound for each blocking handover operation.
pub const DEFAULT_HANDOVER_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum MessageKind {
    Request = 1,
    Item = 2,
    Finished = 3,
    Prepared = 4,
    Commit = 5,
    Abort = 6,
    Challenge = 7,
    ChallengeResponse = 8,
}

impl TryFrom<u8> for MessageKind {
    type Error = HandoverError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Request),
            2 => Ok(Self::Item),
            3 => Ok(Self::Finished),
            4 => Ok(Self::Prepared),
            5 => Ok(Self::Commit),
            6 => Ok(Self::Abort),
            7 => Ok(Self::Challenge),
            8 => Ok(Self::ChallengeResponse),
            _ => Err(HandoverError::Protocol(format!(
                "unknown message kind {value}"
            ))),
        }
    }
}

/// Resource limits applied before allocating or adopting received handover data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HandoverLimits {
    max_payload_size: usize,
    max_fds_per_item: usize,
    max_name_size: usize,
    max_items: usize,
    max_total_payload_size: usize,
    max_total_fds: usize,
}

impl HandoverLimits {
    pub fn new(
        max_payload_size: usize,
        max_fds_per_item: usize,
        max_name_size: usize,
    ) -> Result<Self, HandoverError> {
        if max_payload_size > HARD_MAX_PAYLOAD_SIZE {
            return Err(HandoverError::Limit(format!(
                "payload limit exceeds hard maximum of {HARD_MAX_PAYLOAD_SIZE} bytes"
            )));
        }
        if max_fds_per_item > HARD_MAX_FDS_PER_ITEM {
            return Err(HandoverError::Limit(format!(
                "file descriptor limit exceeds hard maximum of {HARD_MAX_FDS_PER_ITEM}"
            )));
        }
        if max_name_size > HARD_MAX_NAME_SIZE {
            return Err(HandoverError::Limit(format!(
                "name limit exceeds hard maximum of {HARD_MAX_NAME_SIZE} bytes"
            )));
        }
        Ok(Self {
            max_payload_size,
            max_fds_per_item,
            max_name_size,
            ..Self::default()
        })
    }

    /// Set cumulative resource limits for one transaction.
    pub fn with_transaction_limits(
        mut self,
        max_items: usize,
        max_total_payload_size: usize,
        max_total_fds: usize,
    ) -> Result<Self, HandoverError> {
        if max_items > HARD_MAX_ITEMS {
            return Err(HandoverError::Limit(format!(
                "item limit exceeds hard maximum of {HARD_MAX_ITEMS}"
            )));
        }
        if max_total_payload_size > HARD_MAX_TOTAL_PAYLOAD_SIZE {
            return Err(HandoverError::Limit(format!(
                "total payload limit exceeds hard maximum of {HARD_MAX_TOTAL_PAYLOAD_SIZE} bytes"
            )));
        }
        if max_total_fds > HARD_MAX_TOTAL_FDS {
            return Err(HandoverError::Limit(format!(
                "total file descriptor limit exceeds hard maximum of {HARD_MAX_TOTAL_FDS}"
            )));
        }
        self.max_items = max_items;
        self.max_total_payload_size = max_total_payload_size;
        self.max_total_fds = max_total_fds;
        Ok(self)
    }

    pub fn max_payload_size(&self) -> usize {
        self.max_payload_size
    }

    pub fn max_fds_per_item(&self) -> usize {
        self.max_fds_per_item
    }

    pub fn max_name_size(&self) -> usize {
        self.max_name_size
    }

    pub fn max_items(&self) -> usize {
        self.max_items
    }

    pub fn max_total_payload_size(&self) -> usize {
        self.max_total_payload_size
    }

    pub fn max_total_fds(&self) -> usize {
        self.max_total_fds
    }
}

impl Default for HandoverLimits {
    fn default() -> Self {
        Self {
            max_payload_size: HARD_MAX_PAYLOAD_SIZE,
            max_fds_per_item: HARD_MAX_FDS_PER_ITEM,
            max_name_size: HARD_MAX_NAME_SIZE,
            max_items: 1024,
            max_total_payload_size: HARD_MAX_TOTAL_PAYLOAD_SIZE,
            max_total_fds: 1024,
        }
    }
}

#[derive(Clone, Copy, Default)]
struct TransactionUsage {
    items: usize,
    payload_size: usize,
    fds: usize,
}

impl TransactionUsage {
    fn with_item(
        self,
        payload_size: usize,
        fds: usize,
        limits: HandoverLimits,
    ) -> Result<Self, HandoverError> {
        let items = self
            .items
            .checked_add(1)
            .ok_or_else(|| HandoverError::Limit("transaction item count overflowed".into()))?;
        let payload_size = self
            .payload_size
            .checked_add(payload_size)
            .ok_or_else(|| HandoverError::Limit("transaction payload size overflowed".into()))?;
        let fds = self.fds.checked_add(fds).ok_or_else(|| {
            HandoverError::Limit("transaction file descriptor count overflowed".into())
        })?;
        if items > limits.max_items {
            return Err(HandoverError::Limit(format!(
                "transaction exceeds its limit of {} items",
                limits.max_items
            )));
        }
        if payload_size > limits.max_total_payload_size {
            return Err(HandoverError::Limit(format!(
                "transaction exceeds its total payload limit of {} bytes",
                limits.max_total_payload_size
            )));
        }
        if fds > limits.max_total_fds {
            return Err(HandoverError::Limit(format!(
                "transaction exceeds its total limit of {} file descriptors",
                limits.max_total_fds
            )));
        }
        Ok(Self {
            items,
            payload_size,
            fds,
        })
    }
}

/// Inclusive range of application handover protocol versions.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SupportedVersions {
    min: u16,
    max: u16,
}

impl SupportedVersions {
    pub fn new(min: u16, max: u16) -> Result<Self, HandoverError> {
        if min > max {
            return Err(HandoverError::Protocol(format!(
                "minimum application version {min} exceeds maximum {max}"
            )));
        }
        Ok(Self { min, max })
    }

    pub fn exact(version: u16) -> Self {
        Self {
            min: version,
            max: version,
        }
    }

    pub fn min(&self) -> u16 {
        self.min
    }

    pub fn max(&self) -> u16 {
        self.max
    }

    pub fn contains(&self, version: u16) -> bool {
        (self.min..=self.max).contains(&version)
    }
}

/// A received application item and the descriptors attached to it.
#[derive(Debug)]
pub struct HandoverItem {
    name: String,
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
}

impl HandoverItem {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn fds(&self) -> &[OwnedFd] {
        &self.fds
    }

    pub fn into_parts(self) -> (String, Vec<u8>, Vec<OwnedFd>) {
        (self.name, self.payload, self.fds)
    }
}

/// A transport or protocol failure during a handover.
#[derive(Debug)]
#[non_exhaustive]
pub enum HandoverError {
    Io(io::Error),
    Protocol(String),
    Limit(String),
    UnsupportedVersion {
        selected: u16,
        supported: SupportedVersions,
    },
    Aborted(String),
}

impl fmt::Display for HandoverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "handover I/O failed: {error}"),
            Self::Protocol(error) => write!(f, "invalid handover protocol: {error}"),
            Self::Limit(error) => write!(f, "handover limit exceeded: {error}"),
            Self::UnsupportedVersion {
                selected,
                supported,
            } => write!(
                f,
                "application handover version {selected} is outside supported range {}..={}",
                supported.min, supported.max
            ),
            Self::Aborted(reason) => write!(f, "handover aborted: {reason}"),
        }
    }
}

impl std::error::Error for HandoverError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            _ => None,
        }
    }
}

impl From<io::Error> for HandoverError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<nix::Error> for HandoverError {
    fn from(value: nix::Error) -> Self {
        Self::Io(io::Error::from_raw_os_error(value as i32))
    }
}

#[derive(Debug)]
struct Frame {
    kind: MessageKind,
    transaction_id: u64,
    application_version: u16,
    payload: Vec<u8>,
    fds: Vec<OwnedFd>,
}

/// One endpoint of a private handover channel between adjacent process generations.
pub struct HandoverPeer {
    socket: UnixDatagram,
    limits: HandoverLimits,
    readiness_gate: Option<u64>,
}

impl HandoverPeer {
    pub fn new(socket: UnixDatagram) -> io::Result<Self> {
        Self::with_limits(socket, HandoverLimits::default())
    }

    pub fn with_limits(socket: UnixDatagram, limits: HandoverLimits) -> io::Result<Self> {
        let peer = Self {
            socket,
            limits,
            readiness_gate: None,
        };
        peer.set_timeout(Some(DEFAULT_HANDOVER_TIMEOUT))?;
        Ok(peer)
    }

    pub fn limits(&self) -> HandoverLimits {
        self.limits
    }

    pub fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.socket.set_read_timeout(timeout)?;
        self.socket.set_write_timeout(timeout)
    }

    pub fn into_inner(self) -> UnixDatagram {
        self.socket
    }

    pub(crate) fn with_readiness_gate(mut self, readiness_gate: u64) -> Self {
        self.readiness_gate = Some(readiness_gate);
        self
    }

    /// Request application state from the immediately previous process generation.
    pub fn request(
        &mut self,
        versions: SupportedVersions,
    ) -> Result<IncomingHandover<'_>, HandoverError> {
        let transaction_id = random_u64()?;
        let mut payload = Vec::with_capacity(4);
        payload.extend_from_slice(&versions.min.to_be_bytes());
        payload.extend_from_slice(&versions.max.to_be_bytes());
        self.send_frame(MessageKind::Request, transaction_id, 0, &payload, &[])?;
        loop {
            let frame = self.recv_frame()?;
            if frame.transaction_id != transaction_id {
                continue;
            }
            match frame.kind {
                MessageKind::Challenge => {
                    if !frame.fds.is_empty() || frame.payload.len() != 16 {
                        return Err(HandoverError::Protocol(
                            "challenge must contain 16 bytes and no file descriptors".into(),
                        ));
                    }
                    self.send_frame(
                        MessageKind::ChallengeResponse,
                        transaction_id,
                        0,
                        &frame.payload,
                        &[],
                    )?;
                    break;
                }
                MessageKind::Abort => return Err(abort_error(frame)?),
                kind => return Err(unexpected_message(kind, "challenge or abort")),
            }
        }
        Ok(IncomingHandover {
            peer: self,
            transaction_id,
            versions,
            application_version: None,
            finished: false,
            failed: false,
            usage: TransactionUsage::default(),
        })
    }

    /// Wait for a handover request from the immediately following process generation.
    pub fn receive_request(&mut self) -> Result<HandoverRequest<'_>, HandoverError> {
        let mut pending_request = None;
        'request: loop {
            let frame = match pending_request.take() {
                Some(frame) => frame,
                None => loop {
                    let frame = self.recv_frame()?;
                    if frame.kind == MessageKind::Request {
                        break frame;
                    }
                },
            };
            let (transaction_id, versions) = decode_request(frame)?;
            let challenge = random_bytes()?;
            self.send_frame(MessageKind::Challenge, transaction_id, 0, &challenge, &[])?;

            loop {
                let frame = self.recv_frame()?;
                if frame.kind == MessageKind::Request {
                    pending_request = Some(frame);
                    continue 'request;
                }
                if frame.kind != MessageKind::ChallengeResponse
                    || frame.transaction_id != transaction_id
                    || frame.payload != challenge
                {
                    continue;
                }
                if !frame.fds.is_empty() {
                    return Err(HandoverError::Protocol(
                        "challenge response carried file descriptors".into(),
                    ));
                }
                return Ok(HandoverRequest {
                    peer: self,
                    transaction_id,
                    versions,
                });
            }
        }
    }

    fn send_frame(
        &self,
        kind: MessageKind,
        transaction_id: u64,
        application_version: u16,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> Result<(), HandoverError> {
        validate_outgoing(kind, payload, fds, self.limits)?;
        let frame = encode_frame(
            kind,
            transaction_id,
            application_version,
            payload,
            fds.len(),
        );
        send_encoded(self.socket.as_raw_fd(), &frame, fds)
    }

    fn recv_frame(&self) -> Result<Frame, HandoverError> {
        recv_frame(self.socket.as_raw_fd(), self.limits)
    }

    fn recv_transaction_frame(&self, transaction_id: u64) -> Result<Frame, HandoverError> {
        loop {
            let frame = self.recv_frame()?;
            if frame.transaction_id == transaction_id {
                return Ok(frame);
            }
        }
    }
}

/// A child generation's request observed by its parent.
pub struct HandoverRequest<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    versions: SupportedVersions,
}

impl<'a> HandoverRequest<'a> {
    pub fn supported_versions(&self) -> SupportedVersions {
        self.versions
    }

    pub fn begin(self, application_version: u16) -> Result<OutgoingHandover<'a>, HandoverError> {
        if !self.versions.contains(application_version) {
            return Err(HandoverError::UnsupportedVersion {
                selected: application_version,
                supported: self.versions,
            });
        }
        Ok(OutgoingHandover {
            peer: self.peer,
            transaction_id: self.transaction_id,
            application_version,
            usage: TransactionUsage::default(),
        })
    }

    pub fn abort(self, reason: &str) -> Result<(), HandoverError> {
        send_abort(self.peer, self.transaction_id, reason)
    }
}

/// A parent-side transaction that may send one or more state/descriptor items.
pub struct OutgoingHandover<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    application_version: u16,
    usage: TransactionUsage,
}

impl<'a> OutgoingHandover<'a> {
    pub fn application_version(&self) -> u16 {
        self.application_version
    }

    pub fn send_item(
        &mut self,
        name: &str,
        payload: &[u8],
        fds: &[BorrowedFd<'_>],
    ) -> Result<(), HandoverError> {
        if name.len() > self.peer.limits.max_name_size {
            return Err(HandoverError::Limit(format!(
                "item name has {} bytes, maximum is {}",
                name.len(),
                self.peer.limits.max_name_size
            )));
        }
        let encoded_len = 2usize
            .checked_add(name.len())
            .and_then(|length| length.checked_add(payload.len()))
            .ok_or_else(|| HandoverError::Limit("item payload length overflowed".into()))?;
        if encoded_len > self.peer.limits.max_payload_size {
            return Err(HandoverError::Limit(format!(
                "encoded item has {encoded_len} bytes, maximum is {}",
                self.peer.limits.max_payload_size
            )));
        }

        let mut encoded = Vec::with_capacity(encoded_len);
        encoded.extend_from_slice(&(name.len() as u16).to_be_bytes());
        encoded.extend_from_slice(name.as_bytes());
        encoded.extend_from_slice(payload);
        let usage = self
            .usage
            .with_item(encoded.len(), fds.len(), self.peer.limits)?;
        self.peer.send_frame(
            MessageKind::Item,
            self.transaction_id,
            self.application_version,
            &encoded,
            fds,
        )?;
        self.usage = usage;
        Ok(())
    }

    pub fn finish(self) -> Result<AwaitingPrepared<'a>, HandoverError> {
        self.peer.send_frame(
            MessageKind::Finished,
            self.transaction_id,
            self.application_version,
            &[],
            &[],
        )?;
        Ok(AwaitingPrepared {
            peer: self.peer,
            transaction_id: self.transaction_id,
            application_version: self.application_version,
        })
    }

    pub fn abort(self, reason: &str) -> Result<(), HandoverError> {
        send_abort(self.peer, self.transaction_id, reason)
    }
}

/// A completed parent offer waiting for the child to reconstruct dormant state.
#[must_use = "the handover must be committed or aborted"]
pub struct AwaitingPrepared<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    application_version: u16,
}

impl<'a> AwaitingPrepared<'a> {
    pub fn wait(self) -> Result<PreparedHandover<'a>, HandoverError> {
        let frame = self.peer.recv_transaction_frame(self.transaction_id)?;
        match frame.kind {
            MessageKind::Prepared => {
                require_control_frame(&frame, MessageKind::Prepared)?;
                if frame.application_version != self.application_version {
                    return Err(HandoverError::Protocol(format!(
                        "prepared version {} differs from offered version {}",
                        frame.application_version, self.application_version
                    )));
                }
                Ok(PreparedHandover {
                    peer: self.peer,
                    transaction_id: self.transaction_id,
                    application_version: self.application_version,
                })
            }
            MessageKind::Abort => Err(abort_error(frame)?),
            kind => Err(unexpected_message(kind, "prepared or abort")),
        }
    }
}

/// A parent-side handover that the child has reconstructed but not activated.
#[must_use = "the handover must be committed or aborted"]
pub struct PreparedHandover<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    application_version: u16,
}

impl PreparedHandover<'_> {
    /// Commit the handover. After this returns, the parent must relinquish its descriptors and the
    /// child may activate its reconstructed state.
    pub fn commit(self) -> Result<(), HandoverError> {
        self.peer.send_frame(
            MessageKind::Commit,
            self.transaction_id,
            self.application_version,
            &[],
            &[],
        )
    }

    pub fn abort(self, reason: &str) -> Result<(), HandoverError> {
        send_abort(self.peer, self.transaction_id, reason)
    }
}

/// A child-side transaction receiving state from its parent.
pub struct IncomingHandover<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    versions: SupportedVersions,
    application_version: Option<u16>,
    finished: bool,
    failed: bool,
    usage: TransactionUsage,
}

impl<'a> IncomingHandover<'a> {
    pub fn receive_item(&mut self) -> Result<Option<HandoverItem>, HandoverError> {
        if self.failed {
            return Err(HandoverError::Protocol(
                "handover transaction already failed".into(),
            ));
        }
        if self.finished {
            return Ok(None);
        }
        let result = self.receive_item_inner();
        if result.is_err() {
            self.failed = true;
        }
        result
    }

    fn receive_item_inner(&mut self) -> Result<Option<HandoverItem>, HandoverError> {
        let frame = self.peer.recv_transaction_frame(self.transaction_id)?;
        match frame.kind {
            MessageKind::Item => {
                self.observe_version(frame.application_version)?;
                let usage =
                    self.usage
                        .with_item(frame.payload.len(), frame.fds.len(), self.peer.limits)?;
                let item = decode_item(frame, self.peer.limits)?;
                self.usage = usage;
                Ok(Some(item))
            }
            MessageKind::Finished => {
                require_control_frame(&frame, MessageKind::Finished)?;
                self.observe_version(frame.application_version)?;
                self.finished = true;
                Ok(None)
            }
            MessageKind::Abort => Err(abort_error(frame)?),
            kind => Err(unexpected_message(kind, "item, finished, or abort")),
        }
    }

    pub fn application_version(&self) -> Option<u16> {
        self.application_version
    }

    pub fn prepare(self) -> Result<PreparedIncoming<'a>, HandoverError> {
        if self.failed {
            return Err(HandoverError::Protocol(
                "cannot prepare a failed handover transaction".into(),
            ));
        }
        if !self.finished {
            return Err(HandoverError::Protocol(
                "cannot prepare before the finished message".into(),
            ));
        }
        let application_version = self.application_version.ok_or_else(|| {
            HandoverError::Protocol("finished transaction has no application version".into())
        })?;
        self.peer.send_frame(
            MessageKind::Prepared,
            self.transaction_id,
            application_version,
            &[],
            &[],
        )?;
        Ok(PreparedIncoming {
            peer: self.peer,
            transaction_id: self.transaction_id,
            application_version,
        })
    }

    pub fn abort(self, reason: &str) -> Result<(), HandoverError> {
        send_abort(self.peer, self.transaction_id, reason)
    }

    fn observe_version(&mut self, selected: u16) -> Result<(), HandoverError> {
        if !self.versions.contains(selected) {
            return Err(HandoverError::UnsupportedVersion {
                selected,
                supported: self.versions,
            });
        }
        match self.application_version {
            Some(previous) if previous != selected => Err(HandoverError::Protocol(format!(
                "application version changed from {previous} to {selected} during transaction"
            ))),
            Some(_) => Ok(()),
            None => {
                self.application_version = Some(selected);
                Ok(())
            }
        }
    }
}

/// Child-side dormant state waiting for the parent to commit or abort.
#[must_use = "the child must wait for commit before activating handed-over state"]
pub struct PreparedIncoming<'a> {
    peer: &'a mut HandoverPeer,
    transaction_id: u64,
    application_version: u16,
}

/// Proof that the parent committed a child-side handover transaction.
#[must_use = "pass this token to Ecdysis::complete_handover before declaring readiness"]
pub struct HandoverCommit {
    pub(crate) readiness_gate: Option<u64>,
}

impl PreparedIncoming<'_> {
    pub fn wait_for_commit(self) -> Result<HandoverCommit, HandoverError> {
        let frame = self.peer.recv_transaction_frame(self.transaction_id)?;
        match frame.kind {
            MessageKind::Commit => {
                require_control_frame(&frame, MessageKind::Commit)?;
                if frame.application_version != self.application_version {
                    return Err(HandoverError::Protocol(format!(
                        "commit version {} differs from prepared version {}",
                        frame.application_version, self.application_version
                    )));
                }
                Ok(HandoverCommit {
                    readiness_gate: self.peer.readiness_gate,
                })
            }
            MessageKind::Abort => Err(abort_error(frame)?),
            kind => Err(unexpected_message(kind, "commit or abort")),
        }
    }
}

fn random_bytes() -> Result<[u8; 16], HandoverError> {
    let mut bytes = [0; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| HandoverError::Io(io::Error::other(error.to_string())))?;
    Ok(bytes)
}

fn random_u64() -> Result<u64, HandoverError> {
    let mut bytes = [0; 8];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| HandoverError::Io(io::Error::other(error.to_string())))?;
    Ok(u64::from_ne_bytes(bytes))
}

fn decode_request(frame: Frame) -> Result<(u64, SupportedVersions), HandoverError> {
    require_control_frame(&frame, MessageKind::Request)?;
    if frame.payload.len() != 4 {
        return Err(HandoverError::Protocol(format!(
            "request payload has {} bytes, expected 4",
            frame.payload.len()
        )));
    }
    let versions = SupportedVersions::new(
        u16::from_be_bytes([frame.payload[0], frame.payload[1]]),
        u16::from_be_bytes([frame.payload[2], frame.payload[3]]),
    )?;
    Ok((frame.transaction_id, versions))
}

fn send_abort(peer: &HandoverPeer, transaction_id: u64, reason: &str) -> Result<(), HandoverError> {
    peer.send_frame(
        MessageKind::Abort,
        transaction_id,
        0,
        reason.as_bytes(),
        &[],
    )
}

fn validate_outgoing(
    kind: MessageKind,
    payload: &[u8],
    fds: &[BorrowedFd<'_>],
    limits: HandoverLimits,
) -> Result<(), HandoverError> {
    if payload.len() > limits.max_payload_size {
        return Err(HandoverError::Limit(format!(
            "payload has {} bytes, maximum is {}",
            payload.len(),
            limits.max_payload_size
        )));
    }
    if fds.len() > limits.max_fds_per_item {
        return Err(HandoverError::Limit(format!(
            "message has {} file descriptors, maximum is {}",
            fds.len(),
            limits.max_fds_per_item
        )));
    }
    if kind != MessageKind::Item && !fds.is_empty() {
        return Err(HandoverError::Protocol(format!(
            "control message {kind:?} cannot carry file descriptors"
        )));
    }
    Ok(())
}

fn encode_frame(
    kind: MessageKind,
    transaction_id: u64,
    application_version: u16,
    payload: &[u8],
    fd_count: usize,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_LEN + payload.len());
    frame.extend_from_slice(MAGIC);
    frame.push(WIRE_VERSION);
    frame.push(kind as u8);
    frame.extend_from_slice(&0u16.to_be_bytes());
    frame.extend_from_slice(&transaction_id.to_be_bytes());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(&(fd_count as u16).to_be_bytes());
    frame.extend_from_slice(&application_version.to_be_bytes());
    frame.extend_from_slice(payload);
    frame
}

fn send_encoded(socket: RawFd, frame: &[u8], fds: &[BorrowedFd<'_>]) -> Result<(), HandoverError> {
    let raw_fds: Vec<RawFd> = fds.iter().map(AsRawFd::as_raw_fd).collect();
    let cmsgs = if raw_fds.is_empty() {
        Vec::new()
    } else {
        vec![ControlMessage::ScmRights(&raw_fds)]
    };
    let written = sendmsg::<UnixAddr>(socket, &[IoSlice::new(frame)], &cmsgs, send_flags(), None)?;
    if written != frame.len() {
        return Err(HandoverError::Io(io::Error::new(
            io::ErrorKind::WriteZero,
            format!("sent {written} of {} handover bytes", frame.len()),
        )));
    }
    Ok(())
}

fn recv_frame(socket: RawFd, limits: HandoverLimits) -> Result<Frame, HandoverError> {
    let mut bytes = vec![0u8; HEADER_LEN + limits.max_payload_size];
    let (received_bytes, message_flags, raw_fds) = {
        let mut iov = [IoSliceMut::new(&mut bytes)];
        let mut cmsg_buffer = nix::cmsg_space!([RawFd; HARD_MAX_FDS_PER_ITEM]);
        let message = recvmsg::<UnixAddr>(socket, &mut iov, Some(&mut cmsg_buffer), recv_flags())?;
        let raw_fds: Vec<RawFd> = message
            .cmsgs()
            .filter_map(|message| match message {
                ControlMessageOwned::ScmRights(fds) => Some(fds),
                _ => None,
            })
            .flatten()
            .collect();
        (message.bytes, message.flags, raw_fds)
    };

    let fds: Vec<OwnedFd> = raw_fds
        .into_iter()
        .map(|fd| {
            // SAFETY: SCM_RIGHTS created a new descriptor owned by this process.
            unsafe { OwnedFd::from_raw_fd(fd) }
        })
        .collect();
    for fd in &fds {
        set_cloexec(fd.as_raw_fd())?;
    }

    if message_flags.intersects(MsgFlags::MSG_TRUNC | MsgFlags::MSG_CTRUNC) {
        return Err(HandoverError::Limit(
            "received truncated handover data or file descriptors".into(),
        ));
    }
    if received_bytes < HEADER_LEN {
        return Err(HandoverError::Protocol(format!(
            "message has {received_bytes} bytes, shorter than {HEADER_LEN}-byte header"
        )));
    }
    bytes.truncate(received_bytes);
    decode_frame(bytes, fds, limits)
}

fn decode_frame(
    bytes: Vec<u8>,
    fds: Vec<OwnedFd>,
    limits: HandoverLimits,
) -> Result<Frame, HandoverError> {
    if &bytes[0..4] != MAGIC {
        return Err(HandoverError::Protocol("invalid magic".into()));
    }
    if bytes[4] != WIRE_VERSION {
        return Err(HandoverError::Protocol(format!(
            "unsupported wire version {}",
            bytes[4]
        )));
    }
    if bytes[6..8] != [0, 0] {
        return Err(HandoverError::Protocol(
            "reserved header bits are non-zero".into(),
        ));
    }
    let kind = MessageKind::try_from(bytes[5])?;
    let transaction_id = u64::from_be_bytes(bytes[8..16].try_into().unwrap());
    let payload_len = u32::from_be_bytes(bytes[16..20].try_into().unwrap()) as usize;
    let fd_count = u16::from_be_bytes(bytes[20..22].try_into().unwrap()) as usize;
    let application_version = u16::from_be_bytes(bytes[22..24].try_into().unwrap());

    if payload_len > limits.max_payload_size {
        return Err(HandoverError::Limit(format!(
            "declared payload has {payload_len} bytes, maximum is {}",
            limits.max_payload_size
        )));
    }
    if HEADER_LEN + payload_len != bytes.len() {
        return Err(HandoverError::Protocol(format!(
            "declared payload has {payload_len} bytes but message contains {}",
            bytes.len() - HEADER_LEN
        )));
    }
    if fd_count != fds.len() {
        return Err(HandoverError::Protocol(format!(
            "declared {fd_count} file descriptors but received {}",
            fds.len()
        )));
    }
    if fd_count > limits.max_fds_per_item {
        return Err(HandoverError::Limit(format!(
            "received {fd_count} file descriptors, maximum is {}",
            limits.max_fds_per_item
        )));
    }
    if kind != MessageKind::Item && fd_count != 0 {
        return Err(HandoverError::Protocol(format!(
            "control message {kind:?} carried file descriptors"
        )));
    }

    Ok(Frame {
        kind,
        transaction_id,
        application_version,
        payload: bytes[HEADER_LEN..].to_vec(),
        fds,
    })
}

fn decode_item(frame: Frame, limits: HandoverLimits) -> Result<HandoverItem, HandoverError> {
    if frame.payload.len() < 2 {
        return Err(HandoverError::Protocol(
            "item payload is missing its name length".into(),
        ));
    }
    let name_len = u16::from_be_bytes([frame.payload[0], frame.payload[1]]) as usize;
    if name_len > limits.max_name_size {
        return Err(HandoverError::Limit(format!(
            "received item name has {name_len} bytes, maximum is {}",
            limits.max_name_size
        )));
    }
    if 2 + name_len > frame.payload.len() {
        return Err(HandoverError::Protocol(
            "item name extends beyond its payload".into(),
        ));
    }
    let name = std::str::from_utf8(&frame.payload[2..2 + name_len])
        .map_err(|_| HandoverError::Protocol("item name is not UTF-8".into()))?
        .to_owned();
    Ok(HandoverItem {
        name,
        payload: frame.payload[2 + name_len..].to_vec(),
        fds: frame.fds,
    })
}

fn require_control_frame(frame: &Frame, expected: MessageKind) -> Result<(), HandoverError> {
    if frame.kind != expected {
        return Err(unexpected_message(frame.kind, &format!("{expected:?}")));
    }
    if !frame.fds.is_empty()
        || (!matches!(expected, MessageKind::Request | MessageKind::Abort)
            && !frame.payload.is_empty())
    {
        return Err(HandoverError::Protocol(format!(
            "control message {expected:?} contains unexpected data"
        )));
    }
    Ok(())
}

fn abort_error(frame: Frame) -> Result<HandoverError, HandoverError> {
    if !frame.fds.is_empty() {
        return Err(HandoverError::Protocol(
            "abort message carried file descriptors".into(),
        ));
    }
    let reason = String::from_utf8(frame.payload)
        .map_err(|_| HandoverError::Protocol("abort reason is not UTF-8".into()))?;
    Ok(HandoverError::Aborted(reason))
}

fn unexpected_message(kind: MessageKind, expected: &str) -> HandoverError {
    HandoverError::Protocol(format!("received {kind:?} message, expected {expected}"))
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
))]
fn recv_flags() -> MsgFlags {
    MsgFlags::MSG_CMSG_CLOEXEC
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd"
)))]
fn recv_flags() -> MsgFlags {
    MsgFlags::empty()
}

#[cfg(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "haiku",
    target_os = "illumos",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris"
))]
fn send_flags() -> MsgFlags {
    MsgFlags::MSG_NOSIGNAL
}

#[cfg(not(any(
    target_os = "android",
    target_os = "dragonfly",
    target_os = "freebsd",
    target_os = "fuchsia",
    target_os = "haiku",
    target_os = "illumos",
    target_os = "linux",
    target_os = "netbsd",
    target_os = "openbsd",
    target_os = "solaris"
)))]
fn send_flags() -> MsgFlags {
    MsgFlags::empty()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        os::fd::{AsFd, AsRawFd},
        os::unix::net::UnixStream,
        thread,
    };

    use nix::fcntl::{fcntl, FcntlArg, FdFlag};

    use super::*;

    #[test]
    fn transfers_multiple_items_and_commits() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();
        let (mut stream, transferred_stream) = UnixStream::pair().unwrap();

        let child_thread = thread::spawn(move || {
            let mut incoming = child
                .request(SupportedVersions::new(1, 2).unwrap())
                .unwrap();
            let item = incoming.receive_item().unwrap().unwrap();
            assert_eq!(item.name(), "connection");
            assert_eq!(item.payload(), b"state");
            assert_eq!(item.fds().len(), 1);
            let (_, _, mut fds) = item.into_parts();
            let mut transferred = UnixStream::from(fds.remove(0));
            assert!(incoming.receive_item().unwrap().is_none());
            let prepared = incoming.prepare().unwrap();
            let _commit = prepared.wait_for_commit().unwrap();
            transferred.write_all(b"child").unwrap();
        });

        let request = parent.receive_request().unwrap();
        assert_eq!(
            request.supported_versions(),
            SupportedVersions::new(1, 2).unwrap()
        );
        let mut outgoing = request.begin(2).unwrap();
        outgoing
            .send_item("connection", b"state", &[transferred_stream.as_fd()])
            .unwrap();
        let prepared = outgoing.finish().unwrap().wait().unwrap();
        prepared.commit().unwrap();

        let mut response = [0; 5];
        stream.read_exact(&mut response).unwrap();
        assert_eq!(&response, b"child");
        child_thread.join().unwrap();
    }

    #[test]
    fn abort_returns_descriptors_to_parent_control() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();
        let (_stream, transferred_stream) = UnixStream::pair().unwrap();

        let child_thread = thread::spawn(move || {
            let mut incoming = child.request(SupportedVersions::exact(1)).unwrap();
            let item = incoming.receive_item().unwrap().unwrap();
            assert_eq!(item.fds().len(), 1);
            incoming.abort("cannot restore state").unwrap();
        });

        let request = parent.receive_request().unwrap();
        let mut outgoing = request.begin(1).unwrap();
        outgoing
            .send_item("connection", b"state", &[transferred_stream.as_fd()])
            .unwrap();
        let error = match outgoing.finish().unwrap().wait() {
            Ok(_) => panic!("child unexpectedly prepared the handover"),
            Err(error) => error,
        };
        assert!(
            matches!(error, HandoverError::Aborted(ref reason) if reason == "cannot restore state")
        );
        assert!(fcntl(transferred_stream.as_raw_fd(), FcntlArg::F_GETFD).is_ok());
        child_thread.join().unwrap();
    }

    #[test]
    fn received_descriptors_are_close_on_exec() {
        let (sender_socket, receiver_socket) = UnixDatagram::pair().unwrap();
        let sender = HandoverPeer::new(sender_socket).unwrap();
        let receiver = HandoverPeer::new(receiver_socket).unwrap();
        let (_stream, transferred_stream) = UnixStream::pair().unwrap();

        sender
            .send_frame(
                MessageKind::Item,
                1,
                1,
                &[0, 0],
                &[transferred_stream.as_fd()],
            )
            .unwrap();
        let frame = receiver.recv_frame().unwrap();
        let flags = fcntl(frame.fds[0].as_raw_fd(), FcntlArg::F_GETFD).unwrap();
        assert!(FdFlag::from_bits_truncate(flags).contains(FdFlag::FD_CLOEXEC));
    }

    #[test]
    fn rejects_unsupported_application_version_without_sending() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();

        let child_thread = thread::spawn(move || {
            let _incoming = child
                .request(SupportedVersions::new(2, 3).unwrap())
                .unwrap();
        });
        let request = parent.receive_request().unwrap();
        assert!(matches!(
            request.begin(1),
            Err(HandoverError::UnsupportedVersion { selected: 1, .. })
        ));
        child_thread.join().unwrap();
    }

    #[test]
    fn enforces_item_limits() {
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let limits = HandoverLimits::new(8, 1, 3).unwrap();
        let mut peer = HandoverPeer::with_limits(socket, limits).unwrap();
        let mut outgoing = OutgoingHandover {
            peer: &mut peer,
            transaction_id: 1,
            application_version: 1,
            usage: TransactionUsage::default(),
        };
        assert!(matches!(
            outgoing.send_item("long", b"", &[]),
            Err(HandoverError::Limit(_))
        ));
        assert!(matches!(
            outgoing.send_item("ok", b"12345", &[]),
            Err(HandoverError::Limit(_))
        ));
    }

    #[test]
    fn enforces_transaction_limits() {
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let limits = HandoverLimits::new(32, 1, 8)
            .unwrap()
            .with_transaction_limits(1, 32, 1)
            .unwrap();
        let mut peer = HandoverPeer::with_limits(socket, limits).unwrap();
        let mut outgoing = OutgoingHandover {
            peer: &mut peer,
            transaction_id: 1,
            application_version: 1,
            usage: TransactionUsage::default(),
        };
        outgoing.send_item("one", b"state", &[]).unwrap();
        assert!(matches!(
            outgoing.send_item("two", b"state", &[]),
            Err(HandoverError::Limit(_))
        ));
    }

    #[test]
    fn configures_a_default_timeout() {
        let (socket, _peer) = UnixDatagram::pair().unwrap();
        let peer = HandoverPeer::new(socket).unwrap();
        assert_eq!(
            peer.socket.read_timeout().unwrap(),
            Some(DEFAULT_HANDOVER_TIMEOUT)
        );
        assert_eq!(
            peer.socket.write_timeout().unwrap(),
            Some(DEFAULT_HANDOVER_TIMEOUT)
        );
    }

    #[test]
    fn closes_descriptors_from_a_malformed_frame() {
        let (mut stream, transferred_stream) = UnixStream::pair().unwrap();
        let duplicated = nix::unistd::dup(transferred_stream.as_raw_fd()).unwrap();
        // SAFETY: dup returned a new descriptor owned by this process.
        let duplicated = unsafe { OwnedFd::from_raw_fd(duplicated) };
        drop(transferred_stream);
        let frame = encode_frame(MessageKind::Item, 1, 1, &[0, 0], 0);

        assert!(matches!(
            decode_frame(frame, vec![duplicated], HandoverLimits::default()),
            Err(HandoverError::Protocol(_))
        ));
        let mut byte = [0];
        assert_eq!(stream.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn rejects_truncated_datagrams() {
        let (sender, receiver) = UnixDatagram::pair().unwrap();
        let limits = HandoverLimits::new(4, 1, 4).unwrap();
        let receiver = HandoverPeer::with_limits(receiver, limits).unwrap();
        let oversized = encode_frame(MessageKind::Item, 1, 1, &[0; 5], 0);
        sender.send(&oversized).unwrap();

        assert!(matches!(
            receiver.recv_frame(),
            Err(HandoverError::Limit(_))
        ));
    }

    #[test]
    fn prepared_handover_can_still_abort() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();

        let child_thread = thread::spawn(move || {
            let mut incoming = child.request(SupportedVersions::exact(1)).unwrap();
            assert!(incoming.receive_item().unwrap().is_none());
            let error = match incoming.prepare().unwrap().wait_for_commit() {
                Ok(_) => panic!("parent unexpectedly committed the handover"),
                Err(error) => error,
            };
            assert!(
                matches!(error, HandoverError::Aborted(ref reason) if reason == "parent resumed")
            );
        });

        let request = parent.receive_request().unwrap();
        let outgoing = request.begin(1).unwrap();
        let prepared = outgoing.finish().unwrap().wait().unwrap();
        prepared.abort("parent resumed").unwrap();
        child_thread.join().unwrap();
    }

    #[test]
    fn missing_peer_times_out() {
        let (socket, peer) = UnixDatagram::pair().unwrap();
        let mut receiver = HandoverPeer::new(socket).unwrap();
        receiver
            .set_timeout(Some(Duration::from_millis(10)))
            .unwrap();
        drop(peer);

        let error = match receiver.receive_request() {
            Ok(_) => panic!("received a request from a closed peer"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            HandoverError::Io(ref error)
                if matches!(error.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut)
        ));
    }

    #[test]
    fn skips_requests_and_frames_from_a_failed_generation() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let (mut stale_reader, stale_writer) = UnixStream::pair().unwrap();
        let mut stale_request = Vec::new();
        stale_request.extend_from_slice(&SupportedVersions::exact(1).min.to_be_bytes());
        stale_request.extend_from_slice(&SupportedVersions::exact(1).max.to_be_bytes());
        child_socket
            .send(&encode_frame(MessageKind::Request, 1, 0, &stale_request, 0))
            .unwrap();
        child_socket
            .send(&encode_frame(MessageKind::Prepared, 1, 1, &[], 0))
            .unwrap();
        send_encoded(
            child_socket.as_raw_fd(),
            &encode_frame(MessageKind::Item, 1, 1, &[0, 0], 1),
            &[stale_writer.as_fd()],
        )
        .unwrap();
        drop(stale_writer);

        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();
        let child_thread = thread::spawn(move || {
            let mut incoming = child.request(SupportedVersions::exact(2)).unwrap();
            assert!(incoming.receive_item().unwrap().is_none());
            let _commit = incoming.prepare().unwrap().wait_for_commit().unwrap();
        });

        let request = parent.receive_request().unwrap();
        assert_eq!(request.supported_versions(), SupportedVersions::exact(2));
        let outgoing = request.begin(2).unwrap();
        outgoing.finish().unwrap().wait().unwrap().commit().unwrap();
        child_thread.join().unwrap();
        let mut byte = [0];
        assert_eq!(stale_reader.read(&mut byte).unwrap(), 0);
    }

    #[test]
    fn rejected_item_poisoned_the_transaction() {
        let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
        let mut parent = HandoverPeer::new(parent_socket).unwrap();
        let mut child = HandoverPeer::new(child_socket).unwrap();
        let child_thread = thread::spawn(move || {
            let mut incoming = child.request(SupportedVersions::exact(1)).unwrap();
            assert!(matches!(
                incoming.receive_item(),
                Err(HandoverError::Protocol(_))
            ));
            assert!(matches!(
                incoming.receive_item(),
                Err(HandoverError::Protocol(_))
            ));
            assert!(matches!(
                incoming.prepare(),
                Err(HandoverError::Protocol(_))
            ));
        });

        let request = parent.receive_request().unwrap();
        let outgoing = request.begin(1).unwrap();
        outgoing
            .peer
            .send_frame(
                MessageKind::Item,
                outgoing.transaction_id,
                outgoing.application_version,
                &[0],
                &[],
            )
            .unwrap();
        outgoing
            .peer
            .send_frame(
                MessageKind::Finished,
                outgoing.transaction_id,
                outgoing.application_version,
                &[],
                &[],
            )
            .unwrap();
        child_thread.join().unwrap();
    }
}
