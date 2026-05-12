//! ChirpStack gRPC-web device provisioning
//!
//! Registers HARDWARIO STICKER devices in ChirpStack via gRPC-web API.
//! Manual protobuf encoding - same pattern as chirpstack-provision.py.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

const CHIRPSTACK_HOST: &str = "localhost";
const CHIRPSTACK_PORT: u16 = 8080;
const LORAWAN_CONFIG_PATH: &str = "/data/lorawan/config.json";

// --- Minimal Protobuf Encoder ---

fn encode_varint(value: u64) -> Vec<u8> {
    let mut result = Vec::new();
    let mut v = value;
    while v > 0x7F {
        result.push(((v & 0x7F) | 0x80) as u8);
        v >>= 7;
    }
    result.push((v & 0x7F) as u8);
    result
}

fn encode_field(field_number: u32, wire_type: u8, data: &[u8]) -> Vec<u8> {
    let tag = encode_varint(((field_number as u64) << 3) | wire_type as u64);
    let mut result = tag;
    result.extend_from_slice(data);
    result
}

fn encode_string(field_number: u32, value: &str) -> Vec<u8> {
    if value.is_empty() {
        return Vec::new();
    }
    let encoded = value.as_bytes();
    let mut data = encode_varint(encoded.len() as u64);
    data.extend_from_slice(encoded);
    encode_field(field_number, 2, &data)
}

fn encode_submessage(field_number: u32, data: &[u8]) -> Vec<u8> {
    let mut len_prefixed = encode_varint(data.len() as u64);
    len_prefixed.extend_from_slice(data);
    encode_field(field_number, 2, &len_prefixed)
}

/// Decode a varint from data at position. Returns (value, new_pos).
fn decode_varint(data: &[u8], pos: usize) -> Option<(u64, usize)> {
    let mut result: u64 = 0;
    let mut shift = 0;
    let mut p = pos;
    loop {
        if p >= data.len() {
            return None;
        }
        let byte = data[p];
        result |= ((byte & 0x7F) as u64) << shift;
        p += 1;
        if byte & 0x80 == 0 {
            break;
        }
        shift += 7;
    }
    Some((result, p))
}

/// Decode protobuf message into field number -> bytes map (string fields only)
fn decode_message(data: &[u8]) -> std::collections::HashMap<u32, Vec<u8>> {
    let mut fields = std::collections::HashMap::new();
    let mut pos = 0;
    while pos < data.len() {
        let (tag, new_pos) = match decode_varint(data, pos) {
            Some(v) => v,
            None => break,
        };
        pos = new_pos;
        let field_number = (tag >> 3) as u32;
        let wire_type = (tag & 0x07) as u8;

        match wire_type {
            0 => {
                // varint - skip
                match decode_varint(data, pos) {
                    Some((_, new_pos)) => pos = new_pos,
                    None => break,
                }
            }
            2 => {
                // length-delimited
                let (length, new_pos) = match decode_varint(data, pos) {
                    Some(v) => v,
                    None => break,
                };
                pos = new_pos;
                let end = pos + length as usize;
                if end <= data.len() {
                    fields.entry(field_number).or_insert_with(|| data[pos..end].to_vec());
                }
                pos = end;
            }
            5 => pos += 4,  // 32-bit
            1 => pos += 8,  // 64-bit
            _ => break,
        }
    }
    fields
}

// --- gRPC-web Transport ---

fn grpc_web_call(method: &str, request_data: &[u8], token: Option<&str>) -> Result<Option<Vec<u8>>, String> {
    // Frame: 1 byte flags (0x00=data) + 4 bytes big-endian length + payload
    let mut frame = vec![0u8]; // flags = 0 (data frame)
    frame.extend_from_slice(&(request_data.len() as u32).to_be_bytes());
    frame.extend_from_slice(request_data);

    let mut headers = format!(
        "POST /{method} HTTP/1.1\r\n\
         Host: {host}:{port}\r\n\
         Content-Type: application/grpc-web+proto\r\n\
         Accept: application/grpc-web+proto\r\n\
         X-Grpc-Web: 1\r\n\
         Content-Length: {len}\r\n",
        method = method,
        host = CHIRPSTACK_HOST,
        port = CHIRPSTACK_PORT,
        len = frame.len(),
    );
    if let Some(t) = token {
        headers.push_str(&format!("Authorization: Bearer {}\r\n", t));
    }
    headers.push_str("\r\n");

    let addr = format!("{}:{}", CHIRPSTACK_HOST, CHIRPSTACK_PORT);
    let socket_addr = addr.to_socket_addrs()
        .map_err(|e| format!("Failed to resolve ChirpStack address {}: {}", addr, e))?
        .next()
        .ok_or_else(|| format!("No address found for ChirpStack at {}", addr))?;
    let mut stream = TcpStream::connect_timeout(&socket_addr, Duration::from_secs(5))
        .map_err(|e| format!("Failed to connect to ChirpStack at {} (5s timeout): {}", addr, e))?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| format!("Failed to set timeout: {}", e))?;

    stream.write_all(headers.as_bytes())
        .map_err(|e| format!("Failed to send request: {}", e))?;
    stream.write_all(&frame)
        .map_err(|e| format!("Failed to send body: {}", e))?;

    // Read response
    let mut response = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock || e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(format!("Failed to read response: {}", e)),
        }
    }

    // Find body after \r\n\r\n
    let header_end = response.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| "Invalid HTTP response: no header end".to_string())?;

    // Check HTTP status
    let header_str = String::from_utf8_lossy(&response[..header_end]);
    if !header_str.starts_with("HTTP/1.1 200") && !header_str.starts_with("HTTP/1.0 200") {
        let status_line = header_str.lines().next().unwrap_or("");
        return Err(format!("HTTP error: {}", status_line));
    }

    let body = &response[header_end + 4..];

    // Handle chunked transfer encoding
    let body = if header_str.to_lowercase().contains("transfer-encoding: chunked") {
        decode_chunked(body)?
    } else {
        body.to_vec()
    };

    if body.len() < 5 {
        return Ok(None);
    }

    let flags = body[0];
    let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;

    if flags == 0 && length > 0 && body.len() >= 5 + length {
        // Check for error in trailers after data frame
        let trailer_start = 5 + length;
        if trailer_start < body.len() && body[trailer_start] == 0x80 {
            let trailer_data = parse_trailer(&body[trailer_start..])?;
            check_grpc_status(&trailer_data)?;
        }
        return Ok(Some(body[5..5 + length].to_vec()));
    }

    // Trailers-only response (flags=0x80)
    if body[0] == 0x80 {
        let trailer_data = parse_trailer(&body)?;
        check_grpc_status(&trailer_data)?;
    }

    Ok(None)
}

fn parse_trailer(data: &[u8]) -> Result<String, String> {
    if data.len() < 5 || data[0] != 0x80 {
        return Ok(String::new());
    }
    let length = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
    if data.len() >= 5 + length {
        Ok(String::from_utf8_lossy(&data[5..5 + length]).to_string())
    } else {
        Ok(String::new())
    }
}

fn check_grpc_status(trailer_data: &str) -> Result<(), String> {
    if trailer_data.contains("grpc-status:0") || trailer_data.contains("grpc-status: 0") {
        return Ok(());
    }
    if trailer_data.contains("grpc-status:") {
        return Err(format!("gRPC error: {}", trailer_data));
    }
    Ok(())
}

fn decode_chunked(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut result = Vec::new();
    let mut pos = 0;
    loop {
        // Find chunk size line
        let line_end = data[pos..].windows(2)
            .position(|w| w == b"\r\n")
            .map(|p| pos + p);
        let line_end = match line_end {
            Some(e) => e,
            None => break,
        };
        let size_str = std::str::from_utf8(&data[pos..line_end])
            .map_err(|_| "Invalid chunk size".to_string())?
            .trim();
        let chunk_size = usize::from_str_radix(size_str, 16)
            .map_err(|_| format!("Invalid chunk size: {}", size_str))?;
        if chunk_size == 0 {
            break;
        }
        let chunk_start = line_end + 2;
        let chunk_end = chunk_start + chunk_size;
        if chunk_end > data.len() {
            break;
        }
        result.extend_from_slice(&data[chunk_start..chunk_end]);
        pos = chunk_end + 2; // skip trailing \r\n
    }
    Ok(result)
}

// --- ChirpStack API Methods ---

fn login() -> Result<String, String> {
    let req = [
        encode_string(1, "admin"),
        encode_string(2, "admin"),
    ].concat();

    let resp = grpc_web_call("api.InternalService/Login", &req, None)?
        .ok_or_else(|| "Login returned empty response".to_string())?;

    let fields = decode_message(&resp);
    let jwt = fields.get(&1)
        .ok_or_else(|| "No JWT in login response".to_string())?;
    String::from_utf8(jwt.clone())
        .map_err(|_| "Invalid JWT encoding".to_string())
}

fn create_device(
    token: &str,
    dev_eui: &str,
    name: &str,
    description: &str,
    application_id: &str,
    device_profile_id: &str,
) -> Result<(), String> {
    let device = [
        encode_string(1, dev_eui),
        encode_string(2, name),
        encode_string(3, description),
        encode_string(4, application_id),
        encode_string(5, device_profile_id),
    ].concat();

    let req = encode_submessage(1, &device);
    grpc_web_call("api.DeviceService/Create", &req, Some(token))?;
    Ok(())
}

fn delete_device(token: &str, dev_eui: &str) -> Result<(), String> {
    // DeleteDeviceRequest { string dev_eui = 1; } — flat message, no submessage wrap.
    let req = encode_string(1, dev_eui);
    grpc_web_call("api.DeviceService/Delete", &req, Some(token))?;
    Ok(())
}

fn activate_device_abp(
    token: &str,
    dev_eui: &str,
    dev_addr: &str,
    nwk_s_key: &str,
    app_s_key: &str,
) -> Result<(), String> {
    // For LoRaWAN 1.0.x ABP: all three network session keys = nwkskey
    let activation = [
        encode_string(1, dev_eui),
        encode_string(2, dev_addr),
        encode_string(3, app_s_key),     // app_s_key
        encode_string(4, nwk_s_key),     // nwk_s_enc_key
        encode_string(8, nwk_s_key),     // s_nwk_s_int_key
        encode_string(9, nwk_s_key),     // f_nwk_s_int_key
    ].concat();

    let req = encode_submessage(1, &activation);
    grpc_web_call("api.DeviceService/Activate", &req, Some(token))?;
    Ok(())
}

/// Provision a HARDWARIO STICKER in ChirpStack: login + create device + ABP activate.
///
/// Reads application_id and device_profile_id from /data/lorawan/config.json.
pub fn provision_sticker(
    dev_eui: &str,
    name: &str,
    serial_number: &str,
    dev_addr: &str,
    nwk_s_key: &str,
    app_s_key: &str,
) -> Result<(), String> {
    // Read provisioning config
    let config_str = std::fs::read_to_string(LORAWAN_CONFIG_PATH)
        .map_err(|e| format!("Cannot read {}: {}. Has ChirpStack been provisioned?", LORAWAN_CONFIG_PATH, e))?;

    let config: serde_json::Value = serde_json::from_str(&config_str)
        .map_err(|e| format!("Invalid JSON in {}: {}", LORAWAN_CONFIG_PATH, e))?;

    let application_id = config.get("application_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing application_id in {}", LORAWAN_CONFIG_PATH))?;

    let device_profile_id = config.get("device_profile_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("Missing device_profile_id in {}", LORAWAN_CONFIG_PATH))?;

    // Login
    let token = login()?;

    // Create device (handle ALREADY_EXISTS gracefully)
    let description = if serial_number.is_empty() {
        "HARDWARIO STICKER".to_string()
    } else {
        format!("HARDWARIO STICKER S/N: {}", serial_number)
    };

    match create_device(&token, dev_eui, name, &description, application_id, device_profile_id) {
        Ok(()) => {
            eprintln!("[lorawan-provision] Created device {} in ChirpStack", dev_eui);
        }
        Err(e) if e.contains("ALREADY_EXISTS") || e.to_lowercase().contains("already exists") => {
            eprintln!("[lorawan-provision] Device {} already exists in ChirpStack", dev_eui);
        }
        Err(e) => return Err(e),
    }

    // Activate with ABP keys
    activate_device_abp(&token, dev_eui, dev_addr, nwk_s_key, app_s_key)?;
    eprintln!("[lorawan-provision] Activated device {} with ABP keys", dev_eui);

    Ok(())
}

/// Remove a HARDWARIO STICKER from ChirpStack: login + DeviceService/Delete.
/// NOT_FOUND is treated as success (idempotent — device already absent is fine).
pub fn deprovision_sticker(dev_eui: &str) -> Result<(), String> {
    let token = login()?;
    match delete_device(&token, dev_eui) {
        Ok(()) => {
            eprintln!("[lorawan-provision] Deleted device {} from ChirpStack", dev_eui);
            Ok(())
        }
        Err(e) if e.contains("NOT_FOUND")
            || e.to_lowercase().contains("not found")
            || e.contains("grpc-status:5")
            || e.contains("grpc-status: 5") =>
        {
            eprintln!("[lorawan-provision] Device {} already absent in ChirpStack", dev_eui);
            Ok(())
        }
        Err(e) => Err(e),
    }
}
