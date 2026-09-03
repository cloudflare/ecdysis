use std::{
    io::{Read, Write},
    os::fd::AsFd,
    os::unix::net::{UnixDatagram, UnixStream},
    thread,
};

use ecdysis::handover::{HandoverPeer, SupportedVersions};

fn main() {
    let (parent_socket, child_socket) = UnixDatagram::pair().unwrap();
    let mut parent = HandoverPeer::new(parent_socket).unwrap();
    let mut child = HandoverPeer::new(child_socket).unwrap();
    let (mut client, connection) = UnixStream::pair().unwrap();

    let child = thread::spawn(move || {
        let mut incoming = child.request(SupportedVersions::exact(1)).unwrap();
        let item = incoming.receive_item().unwrap().unwrap();
        assert_eq!(item.name(), "connection");
        assert!(incoming.receive_item().unwrap().is_none());

        // Reconstruct state here, but do not poll or otherwise activate it before commit.
        let (_, protocol_state, mut fds) = item.into_parts();
        assert_eq!(protocol_state, b"opaque application state");
        let mut connection = UnixStream::from(fds.remove(0));
        let prepared = incoming.prepare().unwrap();
        let _commit = prepared.wait_for_commit().unwrap();

        connection.write_all(b"new generation").unwrap();
    });

    let request = parent.receive_request().unwrap();
    let mut outgoing = request.begin(1).unwrap();

    // A real application must quiesce the connection before this call and retain enough state to
    // resume it if any operation before commit fails.
    outgoing
        .send_item(
            "connection",
            b"opaque application state",
            &[connection.as_fd()],
        )
        .unwrap();
    let prepared = outgoing.finish().unwrap().wait().unwrap();
    prepared.commit().unwrap();
    drop(connection);

    let mut response = String::new();
    client.read_to_string(&mut response).unwrap();
    assert_eq!(response, "new generation");
    child.join().unwrap();
}
