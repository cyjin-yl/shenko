//! SSH-protocol-in-module guest driver (C06/R018-in-WASM).
//!
//! This guest reads the socket the host admitted and published as
//! `SHENKO_NET_0=<fd>,<host>,<port>,<ops>`, then runs the SSH binary protocol
//! INSIDE the module: it reads the server's `SSH-2.0-...` version banner,
//! sends its own client banner, sends a correctly-framed SSH_MSG_KEXINIT
//! packet (RFC4253 docs/specs/subject-runtime.md#spec-6 framing), and reads the server's KEXINIT reply.
//!
//! The SSH bytes are produced and consumed by this module, not by a host ssh
//! — that is the entire point of the artifact: the protocol runs in WASM,
//! the host only supplies the preopened socket.

use std::env;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

/// Parse `SHENKO_NET_0=<fd>,<host>,<port>,<ops>` and return the fd.
fn socket_fd() -> i32 {
    let value = env::var("SHENKO_NET_0").expect("SHENKO_NET_0 must be published");
    value
        .split(',')
        .next()
        .and_then(|fd| fd.parse::<i32>().ok())
        .expect("SHENKO_NET_0 fd")
}

fn read_line(file: &mut std::fs::File, buf: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut byte = [0u8; 1];
    loop {
        match file.read(&mut byte)? {
            0 => break,
            _ => {
                buf.push(byte[0]);
                if byte[0] == b'\n' || buf.len() >= 255 {
                    break;
                }
            }
        }
    }
    Ok(buf.len())
}

/// Build an SSH_MSG_KEXINIT packet: packet_length(u32) padding_length(u8)
/// payload(msg=20 + cookie(16) + kex lists) padding. Minimal but well-formed.
fn kexinit_packet() -> Vec<u8> {
    let mut payload = Vec::new();
    payload.push(20u8); // SSH_MSG_KEXINIT
    // cookie: 16 bytes (not cryptographically strong here — this milestone is
    // framing, not KEX security; the host supplies entropy in a full port).
    payload.extend_from_slice(&[0x5au8; 16]);
    // kex_algorithms .. languages: minimal name-lists. Offer one each so the
    // server can select. Real KEX follows in a later slice.
    let lists: [&[u8]; 10] = [
        b"curve25519-sha256",                 // kex_algorithms
        b"ssh-ed25519",                       // server_host_key_algorithms
        b"chacha20-poly1305@openssh.com",     // enc c->s
        b"chacha20-poly1305@openssh.com",     // enc s->c
        b"hmac-sha2-256",                     // mac c->s
        b"hmac-sha2-256",                     // mac s->c
        b"none",                              // comp c->s
        b"none",                              // comp s->c
        b"",                                  // languages c->s
        b"",                                  // languages s->c
    ];
    for list in lists {
        payload.extend_from_slice(&(list.len() as u32).to_be_bytes());
        payload.extend_from_slice(list);
    }
    payload.push(0u8); // first_kex_packet_follows = false
    payload.extend_from_slice(&0u32.to_be_bytes()); // reserved

    // packet_length = padding_length + payload + 1; total must be multiple of 8.
    let padding = {
        let base = payload.len() + 5; // 4 (len) + 1 (padlen)
        let pad = (8 - (base % 8)) % 8;
        if pad < 4 { pad + 8 } else { pad }
    };
    let packet_length = (payload.len() + padding + 1) as u32;
    let mut packet = Vec::new();
    packet.extend_from_slice(&packet_length.to_be_bytes());
    packet.push(padding as u8);
    packet.extend_from_slice(&payload);
    packet.extend_from_slice(&vec![0u8; padding]);
    packet
}

fn main() {
    let fd = socket_fd();
    let mut file = unsafe { std::fs::File::from_raw_fd(fd as std::os::fd::RawFd) };

    // 1. Read the server's version banner.
    let mut banner = Vec::new();
    read_line(&mut file, &mut banner).expect("read server banner");
    let banner_str = String::from_utf8_lossy(&banner);
    eprint!("SERVER_BANNER={banner_str}");
    if !banner_str.starts_with("SSH-") {
        eprintln!("BANNER_INVALID");
        std::process::exit(61);
    }

    // 2. Send our client banner.
    file.write_all(b"SSH-2.0-ShenkoWasm_0.1\r\n").expect("send client banner");
    file.flush().expect("flush banner");

    // 3. Send a framed SSH_MSG_KEXINIT.
    let packet = kexinit_packet();
    file.write_all(&packet).expect("send KEXINIT");
    file.flush().expect("flush kexinit");

    // 4. Read the server's KEXINIT reply: 4-byte packet_length first.
    let mut len_buf = [0u8; 4];
    file.read_exact(&mut len_buf).expect("read reply length");
    let packet_length = u32::from_be_bytes(len_buf) as usize;
    let mut rest = vec![0u8; packet_length.min(64 * 1024)];
    file.read_exact(&mut rest).expect("read reply body");
    let padding_length = rest[0] as usize;
    let msg_type = rest.get(1).copied().unwrap_or(0);
    eprintln!(
        "SERVER_KEXINIT_PACKET_LENGTH={packet_length} PADDING={padding_length} MSG_TYPE={msg_type}"
    );
    if msg_type == 20 {
        println!("SSH_EXCHANGE_OK banner={} reply_msg_type=20", banner_str.trim());
    } else {
        eprintln!("REPLY_NOT_KEXINIT type={msg_type}");
        std::process::exit(62);
    }
    // `file` owns the fd the host installed; letting it drop closes the socket
    // cleanly as the guest exits.
}
