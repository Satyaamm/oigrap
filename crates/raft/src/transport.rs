use std::net::{TcpStream};
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::collections::HashMap;
use crate::rpc::{
    AppendEntriesReq, AppendEntriesResp,
    RequestVoteReq, RequestVoteResp,
    InstallSnapshotReq, InstallSnapshotResp,
};
use crate::log::LogEntry;
use crate::node::NodeId;

// ---------------------------------------------------------------------------
// RPC message type tags
// ---------------------------------------------------------------------------

const MSG_APPEND_ENTRIES_REQ: u8 = 1;
const MSG_APPEND_ENTRIES_RESP: u8 = 2;
const MSG_REQUEST_VOTE_REQ: u8 = 3;
const MSG_REQUEST_VOTE_RESP: u8 = 4;
const MSG_INSTALL_SNAPSHOT_REQ: u8 = 5;
const MSG_INSTALL_SNAPSHOT_RESP: u8 = 6;

// ---------------------------------------------------------------------------
// Encoding helpers
// ---------------------------------------------------------------------------

fn write_u8(buf: &mut Vec<u8>, v: u8) {
    buf.push(v);
}

fn write_u32_le(buf: &mut Vec<u8>, v: u32) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_u64_le(buf: &mut Vec<u8>, v: u64) {
    buf.extend_from_slice(&v.to_le_bytes());
}

fn write_bool(buf: &mut Vec<u8>, v: bool) {
    buf.push(if v { 1 } else { 0 });
}

fn write_bytes(buf: &mut Vec<u8>, data: &[u8]) {
    write_u32_le(buf, data.len() as u32);
    buf.extend_from_slice(data);
}

fn write_log_entries(buf: &mut Vec<u8>, entries: &[LogEntry]) {
    write_u32_le(buf, entries.len() as u32);
    for e in entries {
        write_u64_le(buf, e.term);
        write_u64_le(buf, e.index);
        write_bytes(buf, &e.data);
    }
}

// ---------------------------------------------------------------------------
// Decoding helpers
// ---------------------------------------------------------------------------

fn read_u8(data: &[u8], pos: &mut usize) -> Option<u8> {
    if *pos >= data.len() { return None; }
    let v = data[*pos];
    *pos += 1;
    Some(v)
}

fn read_u32_le(data: &[u8], pos: &mut usize) -> Option<u32> {
    let end = pos.checked_add(4)?;
    if end > data.len() { return None; }
    let v = u32::from_le_bytes(data[*pos..end].try_into().ok()?);
    *pos = end;
    Some(v)
}

fn read_u64_le(data: &[u8], pos: &mut usize) -> Option<u64> {
    let end = pos.checked_add(8)?;
    if end > data.len() { return None; }
    let v = u64::from_le_bytes(data[*pos..end].try_into().ok()?);
    *pos = end;
    Some(v)
}

fn read_bool(data: &[u8], pos: &mut usize) -> Option<bool> {
    Some(read_u8(data, pos)? != 0)
}

fn read_bytes(data: &[u8], pos: &mut usize) -> Option<Vec<u8>> {
    let len = read_u32_le(data, pos)? as usize;
    let end = pos.checked_add(len)?;
    if end > data.len() { return None; }
    let v = data[*pos..end].to_vec();
    *pos = end;
    Some(v)
}

fn read_log_entries(data: &[u8], pos: &mut usize) -> Option<Vec<LogEntry>> {
    let count = read_u32_le(data, pos)? as usize;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let term = read_u64_le(data, pos)?;
        let index = read_u64_le(data, pos)?;
        let entry_data = read_bytes(data, pos)?;
        entries.push(LogEntry { term, index, data: entry_data });
    }
    Some(entries)
}

// ---------------------------------------------------------------------------
// Public encode/decode for AppendEntriesReq
// ---------------------------------------------------------------------------

pub fn encode_append_entries_req(req: &AppendEntriesReq) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_APPEND_ENTRIES_REQ);
    write_u64_le(&mut buf, req.term);
    write_u64_le(&mut buf, req.leader_id);
    write_u64_le(&mut buf, req.prev_log_index);
    write_u64_le(&mut buf, req.prev_log_term);
    write_log_entries(&mut buf, &req.entries);
    write_u64_le(&mut buf, req.leader_commit);
    buf
}

pub fn decode_append_entries_req(data: &[u8]) -> Option<AppendEntriesReq> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_APPEND_ENTRIES_REQ { return None; }
    let term = read_u64_le(data, &mut pos)?;
    let leader_id = read_u64_le(data, &mut pos)?;
    let prev_log_index = read_u64_le(data, &mut pos)?;
    let prev_log_term = read_u64_le(data, &mut pos)?;
    let entries = read_log_entries(data, &mut pos)?;
    let leader_commit = read_u64_le(data, &mut pos)?;
    Some(AppendEntriesReq { term, leader_id, prev_log_index, prev_log_term, entries, leader_commit })
}

// ---------------------------------------------------------------------------
// Public encode/decode for AppendEntriesResp
// ---------------------------------------------------------------------------

pub fn encode_append_entries_resp(resp: &AppendEntriesResp) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_APPEND_ENTRIES_RESP);
    write_u64_le(&mut buf, resp.term);
    write_bool(&mut buf, resp.success);
    write_u64_le(&mut buf, resp.match_index);
    buf
}

pub fn decode_append_entries_resp(data: &[u8]) -> Option<AppendEntriesResp> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_APPEND_ENTRIES_RESP { return None; }
    let term = read_u64_le(data, &mut pos)?;
    let success = read_bool(data, &mut pos)?;
    let match_index = read_u64_le(data, &mut pos)?;
    Some(AppendEntriesResp { term, success, match_index })
}

// ---------------------------------------------------------------------------
// Public encode/decode for RequestVoteReq
// ---------------------------------------------------------------------------

pub fn encode_request_vote_req(req: &RequestVoteReq) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_REQUEST_VOTE_REQ);
    write_u64_le(&mut buf, req.term);
    write_u64_le(&mut buf, req.candidate_id);
    write_u64_le(&mut buf, req.last_log_index);
    write_u64_le(&mut buf, req.last_log_term);
    buf
}

pub fn decode_request_vote_req(data: &[u8]) -> Option<RequestVoteReq> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_REQUEST_VOTE_REQ { return None; }
    let term = read_u64_le(data, &mut pos)?;
    let candidate_id = read_u64_le(data, &mut pos)?;
    let last_log_index = read_u64_le(data, &mut pos)?;
    let last_log_term = read_u64_le(data, &mut pos)?;
    Some(RequestVoteReq { term, candidate_id, last_log_index, last_log_term })
}

// ---------------------------------------------------------------------------
// Public encode/decode for RequestVoteResp
// ---------------------------------------------------------------------------

pub fn encode_request_vote_resp(resp: &RequestVoteResp) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_REQUEST_VOTE_RESP);
    write_u64_le(&mut buf, resp.term);
    write_bool(&mut buf, resp.vote_granted);
    buf
}

pub fn decode_request_vote_resp(data: &[u8]) -> Option<RequestVoteResp> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_REQUEST_VOTE_RESP { return None; }
    let term = read_u64_le(data, &mut pos)?;
    let vote_granted = read_bool(data, &mut pos)?;
    Some(RequestVoteResp { term, vote_granted })
}

// ---------------------------------------------------------------------------
// Public encode/decode for InstallSnapshotReq
// ---------------------------------------------------------------------------

pub fn encode_install_snapshot_req(req: &InstallSnapshotReq) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_INSTALL_SNAPSHOT_REQ);
    write_u64_le(&mut buf, req.term);
    write_u64_le(&mut buf, req.leader_id);
    write_u64_le(&mut buf, req.last_included_index);
    write_u64_le(&mut buf, req.last_included_term);
    write_bytes(&mut buf, &req.data);
    buf
}

pub fn decode_install_snapshot_req(data: &[u8]) -> Option<InstallSnapshotReq> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_INSTALL_SNAPSHOT_REQ { return None; }
    let term = read_u64_le(data, &mut pos)?;
    let leader_id = read_u64_le(data, &mut pos)?;
    let last_included_index = read_u64_le(data, &mut pos)?;
    let last_included_term = read_u64_le(data, &mut pos)?;
    let snap_data = read_bytes(data, &mut pos)?;
    Some(InstallSnapshotReq { term, leader_id, last_included_index, last_included_term, data: snap_data })
}

// ---------------------------------------------------------------------------
// Public encode/decode for InstallSnapshotResp
// ---------------------------------------------------------------------------

pub fn encode_install_snapshot_resp(resp: &InstallSnapshotResp) -> Vec<u8> {
    let mut buf = Vec::new();
    write_u8(&mut buf, MSG_INSTALL_SNAPSHOT_RESP);
    write_u64_le(&mut buf, resp.term);
    buf
}

pub fn decode_install_snapshot_resp(data: &[u8]) -> Option<InstallSnapshotResp> {
    let mut pos = 0;
    let tag = read_u8(data, &mut pos)?;
    if tag != MSG_INSTALL_SNAPSHOT_RESP { return None; }
    let term = read_u64_le(data, &mut pos)?;
    Some(InstallSnapshotResp { term })
}

// ---------------------------------------------------------------------------
// Framing: 4-byte LE length prefix + payload
// ---------------------------------------------------------------------------

fn send_framed(stream: &mut TcpStream, payload: &[u8]) -> std::io::Result<()> {
    let len = payload.len() as u32;
    stream.write_all(&len.to_le_bytes())?;
    stream.write_all(payload)?;
    stream.flush()
}

fn recv_framed(stream: &mut TcpStream) -> std::io::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload)?;
    Ok(payload)
}

// ---------------------------------------------------------------------------
// RpcTransport trait
// ---------------------------------------------------------------------------

pub trait RpcTransport: Send + Sync {
    fn send_append_entries(&self, peer: NodeId, addr: &str, req: AppendEntriesReq) -> Option<AppendEntriesResp>;
    fn send_request_vote(&self, peer: NodeId, addr: &str, req: RequestVoteReq) -> Option<RequestVoteResp>;
    fn send_install_snapshot(&self, peer: NodeId, addr: &str, req: InstallSnapshotReq) -> Option<InstallSnapshotResp>;
}

// ---------------------------------------------------------------------------
// TcpTransport
// ---------------------------------------------------------------------------

pub struct TcpTransport {
    #[allow(dead_code)]
    node_id: NodeId,
    conns: Mutex<HashMap<NodeId, TcpStream>>,
}

impl TcpTransport {
    pub fn new(node_id: NodeId) -> Self {
        TcpTransport {
            node_id,
            conns: Mutex::new(HashMap::new()),
        }
    }

    /// Connect (or reuse) a connection to the peer at the given address.
    fn connect(&self, peer: NodeId, addr: &str) -> Option<TcpStream> {
        let mut conns = self.conns.lock().ok()?;

        // Try reusing if stream seems alive by attempting a zero-byte write:
        // we can't trivially probe, so we just keep the cached stream and
        // let the send fail, then reconnect below.
        if conns.contains_key(&peer) {
            // Try to clone the existing stream for use; if clone fails, evict.
            if let Some(existing) = conns.get(&peer) {
                match existing.try_clone() {
                    Ok(cloned) => return Some(cloned),
                    Err(_) => { conns.remove(&peer); }
                }
            }
        }

        // Fresh connection
        match TcpStream::connect(addr) {
            Ok(stream) => {
                // Store a clone in the pool
                if let Ok(pooled) = stream.try_clone() {
                    conns.insert(peer, pooled);
                }
                Some(stream)
            }
            Err(_) => None,
        }
    }

    /// Send an encoded request and receive an encoded response.
    /// On I/O error, evicts the cached connection so next call reconnects.
    fn roundtrip(&self, peer: NodeId, addr: &str, payload: Vec<u8>) -> Option<Vec<u8>> {
        let mut stream = self.connect(peer, addr)?;
        if send_framed(&mut stream, &payload).is_err() {
            // Evict bad connection
            if let Ok(mut conns) = self.conns.lock() {
                conns.remove(&peer);
            }
            // Retry once with a fresh connection
            let mut stream2 = TcpStream::connect(addr).ok()?;
            send_framed(&mut stream2, &payload).ok()?;
            recv_framed(&mut stream2).ok()
        } else {
            match recv_framed(&mut stream) {
                Ok(resp) => Some(resp),
                Err(_) => {
                    if let Ok(mut conns) = self.conns.lock() {
                        conns.remove(&peer);
                    }
                    None
                }
            }
        }
    }
}

impl RpcTransport for TcpTransport {
    fn send_append_entries(&self, peer: NodeId, addr: &str, req: AppendEntriesReq) -> Option<AppendEntriesResp> {
        let payload = encode_append_entries_req(&req);
        let resp_bytes = self.roundtrip(peer, addr, payload)?;
        decode_append_entries_resp(&resp_bytes)
    }

    fn send_request_vote(&self, peer: NodeId, addr: &str, req: RequestVoteReq) -> Option<RequestVoteResp> {
        let payload = encode_request_vote_req(&req);
        let resp_bytes = self.roundtrip(peer, addr, payload)?;
        decode_request_vote_resp(&resp_bytes)
    }

    fn send_install_snapshot(&self, peer: NodeId, addr: &str, req: InstallSnapshotReq) -> Option<InstallSnapshotResp> {
        let payload = encode_install_snapshot_req(&req);
        let resp_bytes = self.roundtrip(peer, addr, payload)?;
        decode_install_snapshot_resp(&resp_bytes)
    }
}

// ---------------------------------------------------------------------------
// Server-side: dispatch an incoming framed message and return a framed response
// ---------------------------------------------------------------------------

/// Dispatch a raw decoded payload through the given node, returning an encoded response.
/// Returns None if the message tag is unrecognised or decoding fails.
pub fn dispatch_rpc(
    payload: &[u8],
    node: &Arc<Mutex<crate::node::RaftNode>>,
) -> Option<Vec<u8>> {
    if payload.is_empty() { return None; }
    match payload[0] {
        MSG_APPEND_ENTRIES_REQ => {
            let req = decode_append_entries_req(payload)?;
            let resp = node.lock().ok()?.handle_append_entries(req);
            Some(encode_append_entries_resp(&resp))
        }
        MSG_REQUEST_VOTE_REQ => {
            let req = decode_request_vote_req(payload)?;
            let resp = node.lock().ok()?.handle_request_vote(req);
            Some(encode_request_vote_resp(&resp))
        }
        MSG_INSTALL_SNAPSHOT_REQ => {
            let req = decode_install_snapshot_req(payload)?;
            let resp = node.lock().ok()?.handle_install_snapshot(req);
            Some(encode_install_snapshot_resp(&resp))
        }
        _ => None,
    }
}

/// Handle one connection: read one framed message, dispatch, write response.
pub fn handle_connection(
    mut stream: TcpStream,
    node: Arc<Mutex<crate::node::RaftNode>>,
) {
    while let Ok(payload) = recv_framed(&mut stream) {
        match dispatch_rpc(&payload, &node) {
            Some(resp) => {
                if send_framed(&mut stream, &resp).is_err() {
                    break;
                }
            }
            None => break,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::log::LogEntry;
    use crate::rpc::{AppendEntriesReq, RequestVoteReq, InstallSnapshotReq};

    #[test]
    fn test_encode_decode_append_entries_req() {
        let entries = vec![
            LogEntry { term: 1, index: 1, data: b"hello".to_vec() },
            LogEntry { term: 2, index: 2, data: b"world".to_vec() },
        ];
        let req = AppendEntriesReq {
            term: 42,
            leader_id: 7,
            prev_log_index: 10,
            prev_log_term: 5,
            entries,
            leader_commit: 9,
        };
        let encoded = encode_append_entries_req(&req);
        let decoded = decode_append_entries_req(&encoded).expect("decode should succeed");

        assert_eq!(decoded.term, req.term);
        assert_eq!(decoded.leader_id, req.leader_id);
        assert_eq!(decoded.prev_log_index, req.prev_log_index);
        assert_eq!(decoded.prev_log_term, req.prev_log_term);
        assert_eq!(decoded.leader_commit, req.leader_commit);
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].term, 1);
        assert_eq!(decoded.entries[0].index, 1);
        assert_eq!(decoded.entries[0].data, b"hello");
        assert_eq!(decoded.entries[1].term, 2);
        assert_eq!(decoded.entries[1].index, 2);
        assert_eq!(decoded.entries[1].data, b"world");
    }

    #[test]
    fn test_encode_decode_append_entries_req_empty_entries() {
        let req = AppendEntriesReq {
            term: 1,
            leader_id: 1,
            prev_log_index: 0,
            prev_log_term: 0,
            entries: vec![],
            leader_commit: 0,
        };
        let encoded = encode_append_entries_req(&req);
        let decoded = decode_append_entries_req(&encoded).expect("decode should succeed");
        assert_eq!(decoded.entries.len(), 0);
        assert_eq!(decoded.term, 1);
    }

    #[test]
    fn test_encode_decode_request_vote() {
        let req = RequestVoteReq {
            term: 3,
            candidate_id: 5,
            last_log_index: 100,
            last_log_term: 2,
        };
        let encoded = encode_request_vote_req(&req);
        let decoded = decode_request_vote_req(&encoded).expect("decode should succeed");

        assert_eq!(decoded.term, req.term);
        assert_eq!(decoded.candidate_id, req.candidate_id);
        assert_eq!(decoded.last_log_index, req.last_log_index);
        assert_eq!(decoded.last_log_term, req.last_log_term);

        // Also test response
        let resp = crate::rpc::RequestVoteResp { term: 3, vote_granted: true };
        let resp_enc = encode_request_vote_resp(&resp);
        let resp_dec = decode_request_vote_resp(&resp_enc).expect("decode resp should succeed");
        assert_eq!(resp_dec.term, 3);
        assert!(resp_dec.vote_granted);

        let resp2 = crate::rpc::RequestVoteResp { term: 3, vote_granted: false };
        let resp2_enc = encode_request_vote_resp(&resp2);
        let resp2_dec = decode_request_vote_resp(&resp2_enc).expect("decode resp2 should succeed");
        assert!(!resp2_dec.vote_granted);
    }

    #[test]
    fn test_encode_decode_install_snapshot() {
        let req = InstallSnapshotReq {
            term: 10,
            leader_id: 1,
            last_included_index: 50,
            last_included_term: 9,
            data: b"snapshot-data-bytes".to_vec(),
        };
        let encoded = encode_install_snapshot_req(&req);
        let decoded = decode_install_snapshot_req(&encoded).expect("decode should succeed");

        assert_eq!(decoded.term, req.term);
        assert_eq!(decoded.leader_id, req.leader_id);
        assert_eq!(decoded.last_included_index, req.last_included_index);
        assert_eq!(decoded.last_included_term, req.last_included_term);
        assert_eq!(decoded.data, req.data);

        // Also test response
        let resp = crate::rpc::InstallSnapshotResp { term: 10 };
        let resp_enc = encode_install_snapshot_resp(&resp);
        let resp_dec = decode_install_snapshot_resp(&resp_enc).expect("decode resp should succeed");
        assert_eq!(resp_dec.term, 10);
    }

    #[test]
    fn test_wrong_tag_returns_none() {
        let req = RequestVoteReq {
            term: 1, candidate_id: 1, last_log_index: 0, last_log_term: 0,
        };
        // Encode as RequestVoteReq but try to decode as AppendEntriesReq
        let encoded = encode_request_vote_req(&req);
        let result = decode_append_entries_req(&encoded);
        assert!(result.is_none(), "wrong tag should yield None");
    }
}
