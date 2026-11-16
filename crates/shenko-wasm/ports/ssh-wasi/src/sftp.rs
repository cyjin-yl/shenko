//! Bounded SFTP v3 file transfers over the authenticated SSH channel. This
//! deliberately implements the wire operations we expose, not shell-based
//! `cat` commands or a native SFTP subprocess.
use anyhow::{Context, Result, bail, ensure};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const CHUNK: usize = 32 * 1024;
const MAX_FRAME: usize = 1024 * 1024;

struct Sftp<S> {
    stream: S,
    next: u32,
}

fn u32_field(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_be_bytes());
}
fn string_field(out: &mut Vec<u8>, value: &[u8]) -> Result<()> {
    u32_field(out, u32::try_from(value.len())?);
    out.extend_from_slice(value);
    Ok(())
}
fn read_u32(input: &mut &[u8]) -> Result<u32> {
    ensure!(input.len() >= 4, "truncated SFTP integer");
    let value = u32::from_be_bytes(input[..4].try_into()?);
    *input = &input[4..];
    Ok(value)
}
fn read_string<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let length = read_u32(input)? as usize;
    ensure!(input.len() >= length, "truncated SFTP string");
    let value = &input[..length];
    *input = &input[length..];
    Ok(value)
}

impl<S: AsyncRead + AsyncWrite + Unpin> Sftp<S> {
    async fn send(&mut self, packet: &[u8]) -> Result<()> {
        ensure!(
            packet.len() <= MAX_FRAME,
            "SFTP request exceeds frame bound"
        );
        self.stream.write_u32(u32::try_from(packet.len())?).await?;
        self.stream.write_all(packet).await?;
        self.stream.flush().await?;
        Ok(())
    }
    async fn receive(&mut self) -> Result<Vec<u8>> {
        let length = self.stream.read_u32().await? as usize;
        ensure!(
            (1..=MAX_FRAME).contains(&length),
            "invalid SFTP frame length {length}"
        );
        let mut packet = vec![0; length];
        self.stream.read_exact(&mut packet).await?;
        Ok(packet)
    }
    async fn new(stream: S) -> Result<Self> {
        let mut this = Self { stream, next: 0 };
        this.send(&[1, 0, 0, 0, 3]).await?;
        let response = this.receive().await?;
        ensure!(response[0] == 2, "SFTP server did not return VERSION");
        let mut fields = &response[1..];
        ensure!(
            read_u32(&mut fields)? == 3,
            "SFTP server did not negotiate version 3"
        );
        // Extension name/value pairs are bounded by the enclosing frame.
        while !fields.is_empty() {
            read_string(&mut fields)?;
            read_string(&mut fields)?;
        }
        Ok(this)
    }
    async fn request(&mut self, kind: u8, fields: &[u8]) -> Result<Vec<u8>> {
        let id = self.next;
        self.next = self
            .next
            .checked_add(1)
            .context("SFTP request IDs exhausted")?;
        let mut packet = vec![kind];
        u32_field(&mut packet, id);
        packet.extend_from_slice(fields);
        self.send(&packet).await?;
        let response = self.receive().await?;
        let mut body = &response[1..];
        ensure!(
            read_u32(&mut body)? == id,
            "SFTP response belongs to a different request"
        );
        Ok(response)
    }
    async fn open(&mut self, path: &str, flags: u32) -> Result<Vec<u8>> {
        let mut fields = Vec::new();
        string_field(&mut fields, path.as_bytes())?;
        u32_field(&mut fields, flags);
        u32_field(&mut fields, 0); // empty ATTRS
        let response = self.request(3, &fields).await?;
        ensure!(
            response[0] == 102,
            "SFTP OPEN failed: {}",
            status(&response)?
        );
        let mut body = &response[5..];
        let handle = read_string(&mut body)?.to_vec();
        ensure!(
            body.is_empty() && !handle.is_empty(),
            "malformed SFTP handle"
        );
        Ok(handle)
    }
    async fn close(&mut self, handle: &[u8]) -> Result<()> {
        let mut fields = Vec::new();
        string_field(&mut fields, handle)?;
        let response = self.request(4, &fields).await?;
        ensure!(status(&response)? == 0, "SFTP CLOSE failed");
        Ok(())
    }
}

fn status(response: &[u8]) -> Result<u32> {
    ensure!(
        response.first() == Some(&101) && response.len() >= 9,
        "unexpected SFTP response"
    );
    let mut fields = &response[5..];
    let code = read_u32(&mut fields)?;
    read_string(&mut fields)?;
    read_string(&mut fields)?;
    ensure!(fields.is_empty(), "malformed SFTP STATUS");
    Ok(code)
}

pub async fn put<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    local: &Path,
    remote: &str,
    overwrite: bool,
) -> Result<()> {
    let mut input = File::open(local)?;
    ensure!(
        input.metadata()?.is_file(),
        "SFTP input is not a regular file"
    );
    let mut sftp = Sftp::new(stream).await?;
    // WRITE|CREAT and either TRUNC (explicitly requested) or EXCL.
    let handle = sftp
        .open(remote, 2 | 8 | if overwrite { 16 } else { 32 })
        .await?;
    let mut offset = 0u64;
    let mut buffer = vec![0; CHUNK];
    loop {
        let count = input.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        let mut fields = Vec::new();
        string_field(&mut fields, &handle)?;
        fields.extend_from_slice(&offset.to_be_bytes());
        string_field(&mut fields, &buffer[..count])?;
        let response = sftp.request(6, &fields).await?;
        ensure!(
            status(&response)? == 0,
            "SFTP WRITE failed; remote file may be partial"
        );
        offset = offset
            .checked_add(count as u64)
            .context("SFTP offset overflow")?;
    }
    sftp.close(&handle).await?;
    println!(
        "{}",
        serde_json::json!({"transferred_bytes":offset,"direction":"put"})
    );
    Ok(())
}

pub async fn get<S: AsyncRead + AsyncWrite + Unpin>(
    stream: S,
    remote: &str,
    local: &Path,
    max_bytes: u64,
) -> Result<()> {
    let mut sftp = Sftp::new(stream).await?;
    let handle = sftp.open(remote, 1).await?;
    // create_new prevents replacement of an existing user file. A process
    // interruption can leave this owned output partial; its failed/unknown
    // native attempt never grants a successful transfer receipt.
    let mut output = super::create_new(local)?;
    let mut offset = 0u64;
    loop {
        let mut fields = Vec::new();
        string_field(&mut fields, &handle)?;
        fields.extend_from_slice(&offset.to_be_bytes());
        u32_field(&mut fields, CHUNK as u32);
        let response = sftp.request(5, &fields).await?;
        match response[0] {
            101 if status(&response)? == 1 => break,
            103 => {
                let mut body = &response[5..];
                let data = read_string(&mut body)?;
                ensure!(
                    body.is_empty() && !data.is_empty() && data.len() <= CHUNK,
                    "malformed SFTP DATA"
                );
                offset = offset
                    .checked_add(data.len() as u64)
                    .context("SFTP offset overflow")?;
                ensure!(
                    offset <= max_bytes,
                    "SFTP download exceeds declared byte limit; local file is partial"
                );
                output.write_all(data)?;
            }
            _ => bail!("SFTP READ failed; local file may be partial"),
        }
    }
    sftp.close(&handle).await?;
    output.sync_all()?;
    println!(
        "{}",
        serde_json::json!({"transferred_bytes":offset,"direction":"get"})
    );
    Ok(())
}
