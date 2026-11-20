//! Subcommand `adus` dijalankan sebagai proses sungguhan, tanpa DATABASE_URL dkk.

use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Output},
    thread,
};

/// Jalankan `adus <args>` dengan env wajib dihapus (sisa dari `.cargo/config.toml`).
fn adus(args: &[&str], bind_addr: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_adus"));
    cmd.args(args).env_remove("ADUS_BIND_ADDR");
    for var in [
        "DATABASE_URL",
        "REDIS_URL",
        "ADUS_SECRET_KEY",
        "ADUS_PUBLIC_URL",
    ] {
        cmd.env_remove(var);
    }
    if let Some(addr) = bind_addr {
        cmd.env("ADUS_BIND_ADDR", addr);
    }
    cmd.output().unwrap()
}

/// Server HTTP satu jawaban di port acak. Mengembalikan alamat `127.0.0.1:port`.
fn serve_once(status_line: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let reply =
            format!("HTTP/1.1 {status_line}\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok");
        stream.write_all(reply.as_bytes()).unwrap();
    });
    addr
}

#[test]
fn healthcheck_exit_code() {
    // 200 → 0, tanpa satu pun env wajib.
    let addr = serve_once("200 OK");
    assert_eq!(adus(&["healthcheck"], Some(&addr)).status.code(), Some(0));

    // 500 → 1.
    let addr = serve_once("500 Internal Server Error");
    assert_eq!(adus(&["healthcheck"], Some(&addr)).status.code(), Some(1));

    // Tidak ada yang mendengarkan → 1.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().to_string()
    };
    assert_eq!(adus(&["healthcheck"], Some(&closed)).status.code(), Some(1));
}

#[test]
fn gen_secret_prints_a_usable_key_without_env() {
    let out = adus(&["gen-secret"], None);
    assert_eq!(out.status.code(), Some(0));
    let key = String::from_utf8(out.stdout).unwrap();
    // 32 byte → 44 karakter base64, satu baris.
    assert_eq!(key.trim().len(), 44);
    assert_eq!(key.trim_end_matches('\n').lines().count(), 1);
}

#[test]
fn check_config_exits_1_on_errors_and_0_when_only_warnings() {
    let out = adus(&["check-config"], None);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stdout).contains("ERROR DATABASE_URL: "));

    let key = String::from_utf8(adus(&["gen-secret"], None).stdout).unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_adus"))
        .arg("check-config")
        .env("DATABASE_URL", "postgres://x")
        .env("REDIS_URL", "redis://x")
        .env("ADUS_SECRET_KEY", key.trim())
        .env("ADUS_PUBLIC_URL", "http://adus.test")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "{text}");
    assert!(text.contains("WARN ADUS_PUBLIC_URL: ") && text.contains("config OK"));
}
