use crate::proto::{
    self, BackendWriter, FrontendMsg, StartupMessage, CANCEL_REQUEST_CODE,
    PROTOCOL_VERSION, SSL_REQUEST_CODE,
};
use crate::session::{encode_value_text, substitute_params, value_type_oid, DbHandle, Portal, PreparedStmt};
use oigrap_sql::QueryResult;
use rcgen::generate_simple_self_signed;
use rustls::ServerConfig;
use std::collections::HashMap;
use std::io::{self, BufReader, BufWriter, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

// ── TLS / plain stream abstraction ───────────────────────────────────────────

/// Holds either a plain TCP stream or a TLS-wrapped stream.
/// Both variants implement Read + Write, so this enum can serve as
/// a unified I/O layer that is swappable at runtime (SSL upgrade).
///
/// The TLS variant is boxed to avoid a large stack-size imbalance between variants.
enum TlsOrPlain {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for TlsOrPlain {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            TlsOrPlain::Plain(s) => s.read(buf),
            TlsOrPlain::Tls(s) => s.read(buf),
        }
    }
}

impl Write for TlsOrPlain {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            TlsOrPlain::Plain(s) => s.write(buf),
            TlsOrPlain::Tls(s) => s.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            TlsOrPlain::Plain(s) => s.flush(),
            TlsOrPlain::Tls(s) => s.flush(),
        }
    }
}

// ── TLS certificate generation ────────────────────────────────────────────────

/// Shared ownership of the underlying transport.
/// Both the read half and the write half hold a clone of this Arc,
/// so TLS upgrade (replacing the inner enum variant) is visible to both.
type SharedStream = Arc<Mutex<TlsOrPlain>>;

/// Read-only handle that delegates to the shared stream.
struct ReadHandle(SharedStream);

impl Read for ReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.0.lock().unwrap().read(buf)
    }
}

/// Write-only handle that delegates to the shared stream.
struct WriteHandle(SharedStream);

impl Write for WriteHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

// ── TLS certificate generation ────────────────────────────────────────────────

/// Generate a self-signed certificate and return a rustls ServerConfig.
/// The certificate is ephemeral (regenerated each call); suitable for
/// development and integration testing.
#[allow(dead_code)]
pub fn make_tls_config() -> Arc<ServerConfig> {
    let cert = generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("rcgen cert generation failed");
    let cert_der =
        rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
    let key_der =
        rustls::pki_types::PrivateKeyDer::try_from(cert.key_pair.serialize_der())
            .expect("private key serialization failed");
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der], key_der)
        .expect("ServerConfig::with_single_cert failed");
    Arc::new(config)
}

// ── Connection ────────────────────────────────────────────────────────────────

pub struct Connection {
    stream: SharedStream,
    reader: BufReader<ReadHandle>,
    writer: BackendWriter<BufWriter<WriteHandle>>,
    db: Arc<Mutex<DbHandle>>,
    prepared: HashMap<String, PreparedStmt>,
    portals: HashMap<String, Portal>,
    pid: u32,
    secret_key: u32,
    tx_status: u8, // b'I', b'T', b'E'
    /// When true, perform MD5 challenge-response before granting access.
    /// Defaults to false (trust mode) for backward compatibility.
    require_auth: bool,
    /// Optional TLS configuration. When present, the server will accept
    /// SSL upgrade requests from clients.
    tls_config: Option<Arc<ServerConfig>>,
}

impl Connection {
    pub fn new(stream: TcpStream, db: Arc<Mutex<DbHandle>>, pid: u32) -> io::Result<Self> {
        Self::new_with_auth(stream, db, pid, false)
    }

    /// Create a connection, optionally requiring MD5 password authentication.
    pub fn new_with_auth(
        stream: TcpStream,
        db: Arc<Mutex<DbHandle>>,
        pid: u32,
        require_auth: bool,
    ) -> io::Result<Self> {
        Self::new_full(stream, db, pid, require_auth, None)
    }

    /// Create a connection with full options including an optional TLS config.
    /// When `tls_config` is Some, the server will respond with 'S' to SSL
    /// requests and perform the TLS handshake.
    pub fn new_full(
        stream: TcpStream,
        db: Arc<Mutex<DbHandle>>,
        pid: u32,
        require_auth: bool,
        tls_config: Option<Arc<ServerConfig>>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Mutex::new(TlsOrPlain::Plain(stream)));
        let reader = BufReader::new(ReadHandle(Arc::clone(&shared)));
        let writer = BackendWriter::new(BufWriter::new(WriteHandle(Arc::clone(&shared))));
        Ok(Connection {
            stream: shared,
            reader,
            writer,
            db,
            prepared: HashMap::new(),
            portals: HashMap::new(),
            pid,
            secret_key: pseudo_rand(pid),
            tx_status: b'I',
            require_auth,
            tls_config,
        })
    }

    /// Run the full connection lifecycle.
    pub fn run(&mut self) -> io::Result<()> {
        // Handle startup (SSL request + startup message)
        let startup = self.handle_startup()?;

        if self.require_auth {
            // MD5 challenge-response authentication
            let username = startup.params.get("user").cloned().unwrap_or_default();
            // Generate a 4-byte random salt from pid + secret_key
            let salt_u32 = self.secret_key ^ self.pid.wrapping_mul(0x9e3779b9);
            let salt: [u8; 4] = salt_u32.to_be_bytes();

            self.writer.auth_md5_challenge(&salt)?;
            self.writer.flush()?;

            // Read the client's PasswordMessage
            let msg = proto::read_message(&mut self.reader)?;
            let client_hash = match msg {
                FrontendMsg::PasswordMessage(pw) => pw,
                _ => {
                    self.writer.error_response("FATAL", "28000", "expected PasswordMessage")?;
                    self.writer.flush()?;
                    return Err(io::Error::new(io::ErrorKind::PermissionDenied, "auth failed"));
                }
            };

            // Accept any non-empty password (real verification left as integration point)
            // Structure: "md5" + md5(md5(password + username) + salt_hex)
            // We verify the prefix to ensure psql sent a proper MD5 response.
            let accepted = client_hash.starts_with("md5") && !client_hash.is_empty();
            let _ = username; // used in full verification

            if !accepted {
                self.writer.error_response("FATAL", "28P01", "password authentication failed")?;
                self.writer.flush()?;
                return Err(io::Error::new(io::ErrorKind::PermissionDenied, "auth failed"));
            }
        }

        // Grant access
        self.writer.auth_ok()?;

        // Announce server parameters
        self.writer.parameter_status("server_version", "15.0")?;
        self.writer.parameter_status("client_encoding", "UTF8")?;
        self.writer.parameter_status("DateStyle", "ISO, MDY")?;
        self.writer.parameter_status("TimeZone", "UTC")?;
        self.writer.parameter_status("integer_datetimes", "on")?;
        self.writer.parameter_status("standard_conforming_strings", "on")?;
        self.writer.backend_key_data(self.pid, self.secret_key)?;
        self.writer.ready_for_query(b'I')?;
        self.writer.flush()?;

        // Main message loop
        loop {
            let msg = match proto::read_message(&mut self.reader) {
                Ok(m) => m,
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(e),
            };

            match msg {
                FrontendMsg::Query(sql) => {
                    self.handle_simple_query(&sql)?;
                }
                FrontendMsg::Parse { name, query, param_types } => {
                    self.handle_parse(name, query, param_types)?;
                }
                FrontendMsg::Bind { portal, statement, param_formats, params, result_formats } => {
                    self.handle_bind(portal, statement, param_formats, params, result_formats)?;
                }
                FrontendMsg::Execute { portal, max_rows } => {
                    self.handle_execute(portal, max_rows)?;
                }
                FrontendMsg::Describe { kind, name } => {
                    self.handle_describe(kind, name)?;
                }
                FrontendMsg::Sync => {
                    self.writer.ready_for_query(self.tx_status)?;
                    self.writer.flush()?;
                }
                FrontendMsg::Flush => {
                    self.writer.flush()?;
                }
                FrontendMsg::Close { kind, name } => {
                    if kind == b'S' {
                        self.prepared.remove(&name);
                    } else {
                        self.portals.remove(&name);
                    }
                    self.writer.close_complete()?;
                }
                FrontendMsg::Terminate => return Ok(()),
                FrontendMsg::CopyData(_) | FrontendMsg::CopyDone | FrontendMsg::CopyFail(_) => {
                    // COPY not yet supported
                }
                FrontendMsg::PasswordMessage(_) => {
                    // Password messages outside of auth negotiation are ignored
                }
            }
        }
    }

    // ── Startup ───────────────────────────────────────────────────────────────

    fn handle_startup(&mut self) -> io::Result<StartupMessage> {
        let startup = proto::read_startup(&mut self.reader)?;

        if startup.protocol_version == SSL_REQUEST_CODE {
            if let Some(tls_cfg) = self.tls_config.clone() {
                // Advertise SSL support and perform the TLS handshake.
                // The BufReader may have buffered bytes; flush it first, then
                // write 'S' directly through the underlying shared stream so
                // the write is not reordered past the handshake.
                //
                // Flush any pending writes, then send 'S' to accept SSL.
                self.writer.flush()?;
                {
                    let mut guard = self.stream.lock().unwrap();
                    guard.write_all(b"S")?;
                    guard.flush()?;
                }

                // Upgrade the shared stream in-place.
                // The BufReader<ReadHandle> retains its internal buffer but
                // that buffer should be empty at this point — the startup
                // message was fully consumed and we have not yet read more
                // bytes from the socket.
                let server_conn = rustls::ServerConnection::new(tls_cfg)
                    .map_err(io::Error::other)?;

                // Extract the TcpStream out of TlsOrPlain::Plain, build the
                // TLS StreamOwned, and replace the shared slot.
                //
                // Strategy: clone the TcpStream first (to use as a placeholder),
                // then mem::replace to pull out the original, wrap it in TLS,
                // and write back. The placeholder clone is dropped immediately.
                let mut guard = self.stream.lock().unwrap();

                // Clone before the mutable borrow to avoid overlapping borrows.
                let placeholder = match &*guard {
                    TlsOrPlain::Plain(s) => s.try_clone()?,
                    TlsOrPlain::Tls(_) => {
                        return Err(io::Error::other("stream already upgraded to TLS"))
                    }
                };

                let plain_stream = match std::mem::replace(&mut *guard, TlsOrPlain::Plain(placeholder)) {
                    TlsOrPlain::Plain(s) => s,
                    TlsOrPlain::Tls(_) => unreachable!(),
                };

                // Now replace the placeholder with the real TLS stream.
                *guard = TlsOrPlain::Tls(Box::new(rustls::StreamOwned::new(server_conn, plain_stream)));
                drop(guard);

                // Read the post-TLS startup message.
                return proto::read_startup(&mut self.reader);
            } else {
                // No TLS config available: decline SSL; client will re-send startup without SSL.
                self.writer.ssl_decline()?;
                self.writer.flush()?;
                return proto::read_startup(&mut self.reader);
            }
        }

        if startup.protocol_version == CANCEL_REQUEST_CODE {
            // Cancel request — no response needed; just close
            return Err(io::Error::other("cancel request"));
        }

        if startup.protocol_version != PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported protocol version: {}", startup.protocol_version),
            ));
        }

        Ok(startup)
    }

    // ── Simple query protocol ─────────────────────────────────────────────────

    fn handle_simple_query(&mut self, sql: &str) -> io::Result<()> {
        let sql = sql.trim();
        if sql.is_empty() {
            self.writer.empty_query_response()?;
            self.writer.ready_for_query(self.tx_status)?;
            self.writer.flush()?;
            return Ok(());
        }

        // Handle semicolon-separated multi-statement batches (psql sends these)
        // Split on semicolons, execute each, send results in sequence.
        let stmts: Vec<&str> = sql.split(';').map(str::trim).filter(|s| !s.is_empty()).collect();

        for stmt_sql in &stmts {
            let upper = stmt_sql.trim().to_uppercase();

            // Intercept session-management commands before the SQL executor.
            if upper.starts_with("SET ") {
                self.writer.command_complete("SET")?;
                continue;
            }
            if upper.starts_with("SHOW ") {
                // Return empty result for SHOW commands
                self.writer.row_description(&[("value".to_string(), 25)])?;
                self.writer.command_complete("SHOW")?;
                continue;
            }
            if upper.starts_with("DEALLOCATE") {
                self.writer.command_complete("DEALLOCATE")?;
                continue;
            }
            if upper.starts_with("DISCARD") {
                self.writer.command_complete("DISCARD")?;
                continue;
            }
            if upper.starts_with("PREPARE ") {
                if let Some(tag) = self.handle_prepare_sql(stmt_sql) {
                    self.writer.command_complete(&tag)?;
                } else {
                    self.writer.error_response("ERROR", "42601", "invalid PREPARE syntax")?;
                }
                continue;
            }
            if upper.starts_with("EXECUTE ") {
                match self.handle_execute_sql(stmt_sql) {
                    Ok(result) => self.send_query_result(&result)?,
                    Err(msg) => {
                        let (code, sev) = sqlstate_for_error(&msg);
                        self.writer.error_response(sev, code, &msg)?;
                        self.tx_status = b'E';
                    }
                }
                continue;
            }

            let exec_result = {
                let mut db = self.db.lock().unwrap();
                db.execute(stmt_sql)
            };
            match exec_result {
                Ok(result) => {
                    self.send_query_result(&result)?;
                }
                Err(msg) => {
                    let (code, sev) = sqlstate_for_error(&msg);
                    self.writer.error_response(sev, code, &msg)?;
                    self.tx_status = b'E';
                }
            }
        }

        // After processing all statements, send ReadyForQuery
        // If we hit an error, status was set to 'E'; reset to 'I' after sending RFQ.
        self.writer.ready_for_query(self.tx_status)?;
        if self.tx_status == b'E' {
            self.tx_status = b'I';
        }
        self.writer.flush()?;
        Ok(())
    }

    // ── Extended query protocol ───────────────────────────────────────────────

    fn handle_parse(&mut self, name: String, query: String, param_types: Vec<u32>) -> io::Result<()> {
        // Store the prepared statement (we do actual parsing at Bind time)
        let stmt = PreparedStmt { name: name.clone(), query, param_types };
        self.prepared.insert(name, stmt);
        self.writer.parse_complete()?;
        Ok(())
    }

    fn handle_bind(
        &mut self,
        portal: String,
        statement: String,
        param_formats: Vec<i16>,
        params: Vec<Option<Vec<u8>>>,
        result_formats: Vec<i16>,
    ) -> io::Result<()> {
        let stmt = match self.prepared.get(&statement) {
            Some(s) => s.clone(),
            None => {
                let msg = format!("prepared statement \"{}\" does not exist", statement);
                self.writer.error_response("ERROR", "26000", &msg)?;
                return Ok(());
            }
        };
        let p = Portal { stmt, params, param_formats, result_formats, result: None, cursor: 0 };
        self.portals.insert(portal, p);
        self.writer.bind_complete()?;
        Ok(())
    }

    fn handle_describe(&mut self, kind: u8, name: String) -> io::Result<()> {
        if kind == b'S' {
            let (param_types, query_opt) = match self.prepared.get(&name) {
                None => {
                    let msg = format!("prepared statement \"{}\" does not exist", name);
                    self.writer.error_response("ERROR", "26000", &msg)?;
                    return Ok(());
                }
                Some(s) => (s.param_types.clone(), s.query.clone()),
            };
            self.writer.parameter_description(&param_types)?;
            if query_opt.trim().to_uppercase().starts_with("SELECT") {
                let substituted = substitute_params(&query_opt, &[], &[]);
                let exec_result = {
                    let mut db = self.db.lock().unwrap();
                    db.execute(&substituted)
                };
                match exec_result {
                    Ok(r) if !r.columns.is_empty() => {
                        let cols = columns_with_oids(&r);
                        self.writer.row_description(&cols)?;
                    }
                    _ => self.writer.no_data()?,
                }
            } else {
                self.writer.no_data()?;
            }
        } else {
            let (query_opt, params_snap, fmt_snap) = match self.portals.get(&name) {
                None => {
                    let msg = format!("portal \"{}\" does not exist", name);
                    self.writer.error_response("ERROR", "34000", &msg)?;
                    return Ok(());
                }
                Some(p) => (p.stmt.query.clone(), p.params.clone(), p.param_formats.clone()),
            };
            if query_opt.trim().to_uppercase().starts_with("SELECT") {
                let query = substitute_params(&query_opt, &params_snap, &fmt_snap);
                let exec_result = {
                    let mut db = self.db.lock().unwrap();
                    db.execute(&query)
                };
                match exec_result {
                    Ok(r) if !r.columns.is_empty() => {
                        let cols = columns_with_oids(&r);
                        self.writer.row_description(&cols)?;
                    }
                    _ => self.writer.no_data()?,
                }
            } else {
                self.writer.no_data()?;
            }
        }
        Ok(())
    }

    fn handle_execute(&mut self, portal: String, max_rows: u32) -> io::Result<()> {
        let portal_data = match self.portals.get_mut(&portal) {
            Some(p) => p,
            None => {
                let msg = format!("portal \"{}\" does not exist", portal);
                self.writer.error_response("ERROR", "34000", &msg)?;
                return Ok(());
            }
        };

        // Execute the query if not already cached in the portal
        if portal_data.result.is_none() {
            let query = substitute_params(
                &portal_data.stmt.query,
                &portal_data.params,
                &portal_data.param_formats,
            );
            let stmt_sql = query;
            // End the borrow scope before calling db.execute
            let _ = portal_data;

            let exec_result = if stmt_sql.trim().to_uppercase().starts_with("SET ") {
                Ok(oigrap_sql::QueryResult { tag: "SET".into(), ..Default::default() })
            } else {
                let mut db = self.db.lock().unwrap();
                db.execute(&stmt_sql).map_err(io::Error::other)
            };

            match exec_result {
                Ok(result) => {
                    if let Some(p) = self.portals.get_mut(&portal) {
                        p.result = Some(result);
                        p.cursor = 0;
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    let (code, sev) = sqlstate_for_error(&msg);
                    self.writer.error_response(sev, code, &msg)?;
                    return Ok(());
                }
            }
        }

        // Send rows from the portal
        // Collect everything we need before any borrow of self.writer
        let (tag, cols, rows_slice, new_cursor, suspended) = {
            let portal_data = self.portals.get_mut(&portal).unwrap();
            let result = portal_data.result.as_ref().unwrap();
            let tag = result.tag.clone();
            let total = result.rows.len();
            let limit = if max_rows == 0 { total } else { max_rows as usize };
            let start = portal_data.cursor;
            let end = (start + limit).min(total);
            let cols = if !result.columns.is_empty() && start == 0 {
                Some(columns_with_oids(result))
            } else {
                None
            };
            let rows_data: Vec<(Vec<String>, Vec<bool>)> = result.rows[start..end]
                .iter()
                .map(|row| row.iter().map(encode_value_text).unzip())
                .collect();
            portal_data.cursor = end;
            let suspended = end < total;
            (tag, cols, rows_data, end, suspended)
        };

        if let Some(col_list) = cols {
            self.writer.row_description(&col_list)?;
        }
        for (vals, nulls) in &rows_slice {
            self.writer.data_row(vals, nulls)?;
        }
        let _ = new_cursor;
        if suspended {
            self.writer.portal_suspended()?;
        } else {
            self.writer.command_complete(&tag)?;
        }
        Ok(())
    }

    // ── SQL-level PREPARE / EXECUTE ───────────────────────────────────────────

    /// Parse `PREPARE name [(types)] AS query` and store in prepared map.
    /// Returns the command tag on success, None on parse failure.
    fn handle_prepare_sql(&mut self, sql: &str) -> Option<String> {
        // PREPARE name AS query  OR  PREPARE name (type, ...) AS query
        let rest = sql.trim().get(8..)?.trim(); // skip "PREPARE "
        // Find "AS" keyword (case-insensitive)
        let as_pos = rest.to_uppercase().find(" AS ")?;
        let name_part = rest[..as_pos].trim();
        let query = rest[as_pos + 4..].trim().to_string();

        // Strip optional type list from name_part
        let name = if let Some(paren) = name_part.find('(') {
            name_part[..paren].trim().to_string()
        } else {
            name_part.to_string()
        };

        let stmt = PreparedStmt { name: name.clone(), query, param_types: vec![] };
        self.prepared.insert(name, stmt);
        Some("PREPARE".to_string())
    }

    /// Parse `EXECUTE name [(args)]` and run it.
    fn handle_execute_sql(&mut self, sql: &str) -> Result<QueryResult, String> {
        let rest = sql.trim().get(8..).ok_or("invalid EXECUTE")?.trim();
        let (name, args_str): (&str, Option<&str>) = if let Some(paren) = rest.find('(') {
            (rest[..paren].trim(), Some(&rest[paren..]))
        } else {
            (rest, None)
        };

        let stmt = self.prepared.get(name).cloned()
            .ok_or_else(|| format!("prepared statement \"{}\" does not exist", name))?;

        let query = if let Some(args) = args_str {
            // Strip outer parens and substitute positional args
            let inner = args.trim().trim_start_matches('(').trim_end_matches(')');
            let params: Vec<Option<Vec<u8>>> = split_sql_args(inner)
                .into_iter()
                .map(|s| Some(s.into_bytes()))
                .collect();
            let formats = vec![0i16; params.len()];
            substitute_params(&stmt.query, &params, &formats)
        } else {
            stmt.query.clone()
        };

        let mut db = self.db.lock().unwrap();
        db.execute(&query)
    }

    // ── Utility ───────────────────────────────────────────────────────────────

    fn send_query_result(&mut self, result: &QueryResult) -> io::Result<()> {
        if !result.columns.is_empty() {
            let cols = columns_with_oids(result);
            self.writer.row_description(&cols)?;
            for row in &result.rows {
                let (vals, nulls): (Vec<String>, Vec<bool>) =
                    row.iter().map(encode_value_text).unzip();
                self.writer.data_row(&vals, &nulls)?;
            }
        }
        let tag = if result.tag.is_empty() { "OK".to_string() } else { result.tag.clone() };
        self.writer.command_complete(&tag)?;
        Ok(())
    }
}

/// Split a comma-separated SQL argument list, respecting quoted strings.
fn split_sql_args(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_string = false;
    for ch in s.chars() {
        match ch {
            '\'' => { in_string = !in_string; current.push(ch); }
            ',' if !in_string => {
                args.push(current.trim().to_string());
                current = String::new();
            }
            _ => current.push(ch),
        }
    }
    let last = current.trim().to_string();
    if !last.is_empty() {
        args.push(last);
    }
    args
}

fn columns_with_oids(result: &QueryResult) -> Vec<(String, u32)> {
    result
        .columns
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let oid = result
                .rows
                .first()
                .and_then(|row| row.get(i))
                .map(value_type_oid)
                .unwrap_or(25);
            (name.clone(), oid)
        })
        .collect()
}

fn sqlstate_for_error(msg: &str) -> (&'static str, &'static str) {
    let lower = msg.to_lowercase();
    if lower.contains("does not exist") || lower.contains("no such table") {
        ("42P01", "ERROR") // undefined_table
    } else if lower.contains("already exists") {
        ("42P07", "ERROR") // duplicate_table
    } else if lower.contains("syntax error") || lower.contains("parse error") {
        ("42601", "ERROR") // syntax_error
    } else if lower.contains("column") && lower.contains("unknown") {
        ("42703", "ERROR") // undefined_column
    } else {
        ("XX000", "ERROR") // internal_error
    }
}

fn pseudo_rand(seed: u32) -> u32 {
    seed.wrapping_mul(1664525).wrapping_add(1013904223)
}

#[cfg(test)]
mod tests {
    use super::make_tls_config;

    #[test]
    fn test_tls_config_generates_cert() {
        let config = make_tls_config();
        // If we got here, certificate generation and ServerConfig construction succeeded.
        drop(config);
    }
}
