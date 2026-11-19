//! Bounded HTTP observation for journeys that start an actual runtime.
use anyhow::{Context, Result, ensure};
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    time::Duration,
};

pub fn health(port: u16) -> Result<Value> {
    http_json(port, "/health", None)
}

pub fn http_json(port: u16, path: &str, response_path: Option<&Path>) -> Result<Value> {
    let mut stream =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_millis(200))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;
    let mut response = Vec::new();
    let read = stream.take(64 * 1024).read_to_end(&mut response);
    if let Some(path) = response_path {
        fs::write(path, &response)?;
    }
    read?;
    ensure!(
        response.len() < 64 * 1024,
        "UI HTTP response exceeded its observation bound"
    );
    let split = response
        .windows(4)
        .position(|bytes| bytes == b"\r\n\r\n")
        .context("UI HTTP headers")?;
    ensure!(response.starts_with(b"HTTP/1.1 200 "), "UI HTTP status");
    Ok(serde_json::from_slice(&response[split + 4..])?)
}
