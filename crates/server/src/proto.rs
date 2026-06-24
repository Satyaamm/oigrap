/// PostgreSQL wire protocol v3 message encoding and decoding.
///
/// All multi-byte integers are big-endian per the protocol spec.
use std::collections::HashMap;
use std::io::{self, Read, Write};

// ── Startup ──────────────────────────────────────────────────────────────────

pub const PROTOCOL_VERSION: u32 = 196608; // 3.0
pub const SSL_REQUEST_CODE: u32 = 80877103;
pub const CANCEL_REQUEST_CODE: u32 = 80877102;

#[allow(dead_code)]
pub struct StartupMessage {
    pub protocol_version: u32,
    pub params: HashMap<String, String>,
}

/// Read the startup message (no type byte, just length + payload).
pub fn read_startup<R: Read>(r: &mut R) -> io::Result<StartupMessage> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let length = u32::from_be_bytes(len_buf) as usize;

    if length < 8 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "startup message too short"));
    }

    let mut payload = vec![0u8; length - 4];
    r.read_exact(&mut payload)?;

    let protocol_version = u32::from_be_bytes(payload[..4].try_into().unwrap());

    let mut params = HashMap::new();
    if protocol_version != SSL_REQUEST_CODE && protocol_version != CANCEL_REQUEST_CODE {
        let mut pos = 4;
        while pos < payload.len() {
            let key = read_cstr(&payload, &mut pos);
            if key.is_empty() {
                break;
            }
            let val = read_cstr(&payload, &mut pos);
            params.insert(key, val);
        }
    }

    Ok(StartupMessage { protocol_version, params })
}

// ── Frontend messages ─────────────────────────────────────────────────────────

#[derive(Debug)]
#[allow(dead_code)]
pub enum FrontendMsg {
    Query(String),
    Parse { name: String, query: String, param_types: Vec<u32> },
    Bind {
        portal: String,
        statement: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    },
    Execute { portal: String, max_rows: u32 },
    Describe { kind: u8, name: String },
    Sync,
    Flush,
    Close { kind: u8, name: String },
    Terminate,
    CopyData(Vec<u8>),
    CopyDone,
    CopyFail(String),
    /// 'p' — PasswordMessage (used in MD5 auth exchange)
    PasswordMessage(String),
}

/// Read one regular frontend message (type byte + length + payload).
pub fn read_message<R: Read>(r: &mut R) -> io::Result<FrontendMsg> {
    let mut type_buf = [0u8; 1];
    r.read_exact(&mut type_buf)?;
    let msg_type = type_buf[0];

    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf)?;
    let length = u32::from_be_bytes(len_buf) as usize;

    if length < 4 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "message length too short"));
    }
    let payload_len = length - 4;
    let mut payload = vec![0u8; payload_len];
    if payload_len > 0 {
        r.read_exact(&mut payload)?;
    }

    parse_frontend_message(msg_type, &payload)
}

fn parse_frontend_message(msg_type: u8, payload: &[u8]) -> io::Result<FrontendMsg> {
    match msg_type {
        b'Q' => {
            let sql = cstr_from_payload(payload);
            Ok(FrontendMsg::Query(sql))
        }
        b'P' => {
            let mut pos = 0;
            let name = read_cstr(payload, &mut pos);
            let query = read_cstr(payload, &mut pos);
            let nparams = if pos + 2 <= payload.len() {
                u16::from_be_bytes(payload[pos..pos + 2].try_into().unwrap()) as usize
            } else {
                0
            };
            pos += 2;
            let mut param_types = Vec::with_capacity(nparams);
            for _ in 0..nparams {
                if pos + 4 <= payload.len() {
                    param_types.push(u32::from_be_bytes(payload[pos..pos + 4].try_into().unwrap()));
                    pos += 4;
                }
            }
            Ok(FrontendMsg::Parse { name, query, param_types })
        }
        b'B' => {
            let mut pos = 0;
            let portal = read_cstr(payload, &mut pos);
            let statement = read_cstr(payload, &mut pos);

            let nfmt = read_u16(payload, &mut pos) as usize;
            let mut param_formats = Vec::with_capacity(nfmt);
            for _ in 0..nfmt {
                param_formats.push(read_i16(payload, &mut pos));
            }

            let nparams = read_u16(payload, &mut pos) as usize;
            let mut params = Vec::with_capacity(nparams);
            for _ in 0..nparams {
                let plen = read_i32(payload, &mut pos);
                if plen < 0 {
                    params.push(None);
                } else {
                    let len = plen as usize;
                    let bytes = payload[pos..pos + len].to_vec();
                    pos += len;
                    params.push(Some(bytes));
                }
            }

            let nresult = read_u16(payload, &mut pos) as usize;
            let mut result_formats = Vec::with_capacity(nresult);
            for _ in 0..nresult {
                result_formats.push(read_i16(payload, &mut pos));
            }

            Ok(FrontendMsg::Bind { portal, statement, param_formats, params, result_formats })
        }
        b'E' => {
            let mut pos = 0;
            let portal = read_cstr(payload, &mut pos);
            let max_rows = if pos + 4 <= payload.len() {
                u32::from_be_bytes(payload[pos..pos + 4].try_into().unwrap())
            } else {
                0
            };
            Ok(FrontendMsg::Execute { portal, max_rows })
        }
        b'D' => {
            if payload.is_empty() {
                return Ok(FrontendMsg::Describe { kind: 0, name: String::new() });
            }
            let kind = payload[0];
            let mut pos = 1;
            let name = read_cstr(payload, &mut pos);
            Ok(FrontendMsg::Describe { kind, name })
        }
        b'S' => Ok(FrontendMsg::Sync),
        b'H' => Ok(FrontendMsg::Flush),
        b'C' => {
            if payload.is_empty() {
                return Ok(FrontendMsg::Close { kind: 0, name: String::new() });
            }
            let kind = payload[0];
            let mut pos = 1;
            let name = read_cstr(payload, &mut pos);
            Ok(FrontendMsg::Close { kind, name })
        }
        b'X' => Ok(FrontendMsg::Terminate),
        b'p' => Ok(FrontendMsg::PasswordMessage(cstr_from_payload(payload))),
        b'd' => Ok(FrontendMsg::CopyData(payload.to_vec())),
        b'c' => Ok(FrontendMsg::CopyDone),
        b'f' => Ok(FrontendMsg::CopyFail(cstr_from_payload(payload))),
        other => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("unknown frontend message type: {}", other as char),
        )),
    }
}

// ── Backend message writers ───────────────────────────────────────────────────

pub struct BackendWriter<W: Write> {
    w: W,
}

impl<W: Write> BackendWriter<W> {
    pub fn new(w: W) -> Self {
        BackendWriter { w }
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }

    /// 'N' — decline SSL
    pub fn ssl_decline(&mut self) -> io::Result<()> {
        self.w.write_all(b"N")
    }

    /// 'R' code=0 — AuthenticationOk
    pub fn auth_ok(&mut self) -> io::Result<()> {
        self.send_msg(b'R', &0u32.to_be_bytes())
    }

    /// 'R' code=5 — AuthenticationMD5Password with 4-byte salt
    pub fn auth_md5_challenge(&mut self, salt: &[u8; 4]) -> io::Result<()> {
        let mut body = [0u8; 8];
        // code = 5 in big-endian
        body[0..4].copy_from_slice(&5u32.to_be_bytes());
        body[4..8].copy_from_slice(salt);
        self.send_msg(b'R', &body)
    }

    /// 'S' — ParameterStatus
    pub fn parameter_status(&mut self, name: &str, value: &str) -> io::Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(name.as_bytes());
        body.push(0);
        body.extend_from_slice(value.as_bytes());
        body.push(0);
        self.send_msg(b'S', &body)
    }

    /// 'K' — BackendKeyData (pid + secret_key)
    pub fn backend_key_data(&mut self, pid: u32, secret_key: u32) -> io::Result<()> {
        let mut body = [0u8; 8];
        body[..4].copy_from_slice(&pid.to_be_bytes());
        body[4..].copy_from_slice(&secret_key.to_be_bytes());
        self.send_msg(b'K', &body)
    }

    /// 'Z' — ReadyForQuery  status: 'I'=idle 'T'=transaction 'E'=error
    pub fn ready_for_query(&mut self, status: u8) -> io::Result<()> {
        self.send_msg(b'Z', &[status])
    }

    /// 'T' — RowDescription
    pub fn row_description(&mut self, columns: &[(String, u32)]) -> io::Result<()> {
        let mut body = Vec::new();
        let n = columns.len() as u16;
        body.extend_from_slice(&n.to_be_bytes());
        for (name, type_oid) in columns {
            body.extend_from_slice(name.as_bytes());
            body.push(0); // null terminator
            body.extend_from_slice(&0u32.to_be_bytes()); // table OID
            body.extend_from_slice(&0u16.to_be_bytes()); // col attr num
            body.extend_from_slice(&type_oid.to_be_bytes()); // type OID
            let type_size: i16 = match *type_oid {
                16 | 18 => 1,   // bool, char
                20 => 8,        // int8
                23 => 4,        // int4
                700 => 4,       // float4
                701 => 8,       // float8
                _ => -1,        // variable
            };
            body.extend_from_slice(&type_size.to_be_bytes());
            body.extend_from_slice(&(-1i32).to_be_bytes()); // type modifier
            body.extend_from_slice(&0u16.to_be_bytes()); // format_code=0 (text)
        }
        self.send_msg(b'T', &body)
    }

    /// 'D' — DataRow
    pub fn data_row(&mut self, values: &[String], nulls: &[bool]) -> io::Result<()> {
        let mut body = Vec::new();
        let n = values.len() as u16;
        body.extend_from_slice(&n.to_be_bytes());
        for (i, val) in values.iter().enumerate() {
            if nulls[i] {
                body.extend_from_slice(&(-1i32).to_be_bytes());
            } else {
                let bytes = val.as_bytes();
                body.extend_from_slice(&(bytes.len() as i32).to_be_bytes());
                body.extend_from_slice(bytes);
            }
        }
        self.send_msg(b'D', &body)
    }

    /// 'C' — CommandComplete
    pub fn command_complete(&mut self, tag: &str) -> io::Result<()> {
        let mut body = tag.as_bytes().to_vec();
        body.push(0);
        self.send_msg(b'C', &body)
    }

    /// 'E' — ErrorResponse
    pub fn error_response(&mut self, severity: &str, sqlstate: &str, msg: &str) -> io::Result<()> {
        let mut body = Vec::new();
        write_field(&mut body, b'S', severity);
        write_field(&mut body, b'V', severity);
        write_field(&mut body, b'C', sqlstate);
        write_field(&mut body, b'M', msg);
        body.push(0); // terminator
        self.send_msg(b'E', &body)
    }

    /// 'N' — NoticeResponse (same structure as ErrorResponse, different type byte)
    #[allow(dead_code)]
    pub fn notice_response(&mut self, msg: &str) -> io::Result<()> {
        let mut body = Vec::new();
        write_field(&mut body, b'S', "NOTICE");
        write_field(&mut body, b'V', "NOTICE");
        write_field(&mut body, b'C', "00000");
        write_field(&mut body, b'M', msg);
        body.push(0);
        self.send_msg(b'N', &body)
    }

    /// '1' — ParseComplete
    pub fn parse_complete(&mut self) -> io::Result<()> {
        self.send_msg(b'1', &[])
    }

    /// '2' — BindComplete
    pub fn bind_complete(&mut self) -> io::Result<()> {
        self.send_msg(b'2', &[])
    }

    /// 'n' — NoData
    pub fn no_data(&mut self) -> io::Result<()> {
        self.send_msg(b'n', &[])
    }

    /// 'I' — EmptyQueryResponse
    pub fn empty_query_response(&mut self) -> io::Result<()> {
        self.send_msg(b'I', &[])
    }

    /// '3' — CloseComplete
    pub fn close_complete(&mut self) -> io::Result<()> {
        self.send_msg(b'3', &[])
    }

    /// 't' — ParameterDescription (for extended query Describe)
    pub fn parameter_description(&mut self, param_types: &[u32]) -> io::Result<()> {
        let mut body = Vec::new();
        body.extend_from_slice(&(param_types.len() as u16).to_be_bytes());
        for oid in param_types {
            body.extend_from_slice(&oid.to_be_bytes());
        }
        self.send_msg(b't', &body)
    }

    /// 's' — PortalSuspended (Execute hit max_rows limit)
    pub fn portal_suspended(&mut self) -> io::Result<()> {
        self.send_msg(b's', &[])
    }

    fn send_msg(&mut self, msg_type: u8, body: &[u8]) -> io::Result<()> {
        let length = (body.len() + 4) as u32;
        self.w.write_all(&[msg_type])?;
        self.w.write_all(&length.to_be_bytes())?;
        self.w.write_all(body)
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn read_cstr(data: &[u8], pos: &mut usize) -> String {
    let start = *pos;
    while *pos < data.len() && data[*pos] != 0 {
        *pos += 1;
    }
    let s = std::str::from_utf8(&data[start..*pos]).unwrap_or("").to_string();
    if *pos < data.len() {
        *pos += 1; // skip null
    }
    s
}

fn cstr_from_payload(payload: &[u8]) -> String {
    let end = payload.iter().position(|&b| b == 0).unwrap_or(payload.len());
    std::str::from_utf8(&payload[..end]).unwrap_or("").to_string()
}

fn read_u16(data: &[u8], pos: &mut usize) -> u16 {
    if *pos + 2 > data.len() {
        return 0;
    }
    let v = u16::from_be_bytes(data[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    v
}

fn read_i16(data: &[u8], pos: &mut usize) -> i16 {
    if *pos + 2 > data.len() {
        return 0;
    }
    let v = i16::from_be_bytes(data[*pos..*pos + 2].try_into().unwrap());
    *pos += 2;
    v
}

fn read_i32(data: &[u8], pos: &mut usize) -> i32 {
    if *pos + 4 > data.len() {
        return -1;
    }
    let v = i32::from_be_bytes(data[*pos..*pos + 4].try_into().unwrap());
    *pos += 4;
    v
}

fn write_field(buf: &mut Vec<u8>, code: u8, value: &str) {
    buf.push(code);
    buf.extend_from_slice(value.as_bytes());
    buf.push(0);
}
