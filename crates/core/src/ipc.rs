use std::io::{BufRead, BufReader, Read, Write};
use std::sync::Arc;
use std::thread;

use anyhow::{Context, Result};
use interprocess::local_socket::{GenericNamespaced, ListenerOptions, Stream, prelude::*};
use interprocess::os::windows::local_socket::ListenerOptionsExt;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use sha2::{Digest, Sha256};
use widestring::U16CString;
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::manager::RecorderManager;
use crate::model::{BridgeRequest, BridgeResponse};

const MAX_IPC_MESSAGE_BYTES: u64 = 16 * 1024 * 1024;

pub fn pipe_name() -> String {
    let user = std::env::var("USERNAME").unwrap_or_else(|_| "windows-user".into());
    let mut hash = Sha256::new();
    hash.update(user.as_bytes());
    let digest = format!("{:x}", hash.finalize());
    format!("cdxvidext-v1-{}.sock", &digest[..16])
}

pub fn send_request(request: &BridgeRequest) -> Result<BridgeResponse> {
    let name = pipe_name().to_ns_name::<GenericNamespaced>()?;
    let mut reader =
        BufReader::new(Stream::connect(name).context("Flight Recorder is not running")?);
    let mut line = serde_json::to_vec(request)?;
    line.push(b'\n');
    reader.get_mut().write_all(&line)?;
    reader.get_mut().flush()?;
    let mut response = String::new();
    Read::take(&mut reader, MAX_IPC_MESSAGE_BYTES + 1).read_line(&mut response)?;
    if response.len() as u64 > MAX_IPC_MESSAGE_BYTES {
        anyhow::bail!("recorder response exceeded the IPC size limit");
    }
    serde_json::from_str(&response).context("recorder returned an invalid pipe response")
}

pub fn serve(manager: Arc<RecorderManager>) -> Result<()> {
    let name = pipe_name().to_ns_name::<GenericNamespaced>()?;
    // Restrict the named pipe to its owning Windows user (plus LocalSystem).
    // Same-user processes are intentionally inside the documented trust boundary.
    let sddl = U16CString::from_str("D:P(A;;GA;;;OW)(A;;GA;;;SY)")?;
    let security_descriptor = SecurityDescriptor::deserialize(&sddl)?;
    let listener = ListenerOptions::new()
        .name(name)
        .security_descriptor(security_descriptor)
        .create_sync()
        .context("the recorder pipe is already in use")?;
    for connection in listener.incoming() {
        let manager = manager.clone();
        match connection {
            Ok(connection) => {
                thread::spawn(move || {
                    let _ = handle_connection(connection, manager);
                });
            }
            Err(error) => tracing::warn!(%error, "named pipe connection failed"),
        }
    }
    Ok(())
}

fn handle_connection(connection: Stream, manager: Arc<RecorderManager>) -> Result<()> {
    let peer_pid = connection
        .peer_creds()?
        .pid()
        .context("named-pipe peer did not expose a process id")?;
    let mut peer_session = 0_u32;
    let mut current_session = 0_u32;
    unsafe {
        ProcessIdToSessionId(peer_pid, &mut peer_session)?;
        ProcessIdToSessionId(GetCurrentProcessId(), &mut current_session)?;
    }
    if peer_session != current_session {
        anyhow::bail!("rejected named-pipe client from another Windows logon session");
    }
    let mut reader = BufReader::new(connection);
    let mut line = String::new();
    Read::take(&mut reader, MAX_IPC_MESSAGE_BYTES + 1).read_line(&mut line)?;
    if line.len() as u64 > MAX_IPC_MESSAGE_BYTES {
        anyhow::bail!("recorder request exceeded the IPC size limit");
    }
    let response = match serde_json::from_str::<BridgeRequest>(&line) {
        Ok(request) => manager.handle_request(request),
        Err(error) => BridgeResponse::failure(format!("invalid request: {error}")),
    };
    serde_json::to_writer(&mut *reader.get_mut(), &response)?;
    reader.get_mut().write_all(b"\n")?;
    reader.get_mut().flush()?;
    Ok(())
}
