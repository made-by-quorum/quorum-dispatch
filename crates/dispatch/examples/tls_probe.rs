//! Live TLS probe for the archive S3 client (Atomic D acceptance):
//! `AWS_ACCESS_KEY_ID=... AWS_SECRET_ACCESS_KEY=... \
//!    cargo run --example tls_probe -- <endpoint> <bucket> <region> <key>`
//! GETs one object over https and prints status/size. With
//! `--negative <host:port>` instead, asserts a TLS handshake against a
//! plaintext port fails LOUDLY (no silent fallback).

use dispatch::archive::credentials::S3Credentials;
use dispatch::archive::s3::S3Client;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let fake = || S3Credentials {
        access_key_id: "x".into(),
        secret_access_key: "x".into(),
        session_token: None,
    };
    if args.len() == 3 && args[1] == "--negative" {
        let client = S3Client::new(&format!("https://{}", args[2]), "b", "garage", fake())
            .expect("client construction");
        match client.get_object("nope", "20260719T000000Z") {
            Err(e) => println!("NEGATIVE OK (loud failure as required): {e}"),
            Ok(_) => println!("NEGATIVE FAILED: plaintext port accepted TLS?!"),
        }
        return;
    }
    let (endpoint, bucket, region, key) = (&args[1], &args[2], &args[3], &args[4]);
    let creds = S3Credentials {
        access_key_id: std::env::var("AWS_ACCESS_KEY_ID").expect("AWS_ACCESS_KEY_ID"),
        secret_access_key: std::env::var("AWS_SECRET_ACCESS_KEY").expect("AWS_SECRET_ACCESS_KEY"),
        session_token: None,
    };
    let client = S3Client::new(endpoint, bucket, region, creds).expect("client construction");
    let now = utc_now_amz();
    match client.get_object(key, &now) {
        Ok(Some(body)) => {
            println!("TLS GET OK: {} bytes from {endpoint}/{bucket}/{key}", body.len())
        }
        Ok(None) => println!("TLS GET OK (404 clean-absent) from {endpoint}/{bucket}/{key}"),
        Err(e) => {
            println!("TLS GET FAILED: {e}");
            std::process::exit(1);
        }
    }
}

/// `YYYYMMDD'T'HHMMSS'Z'` without a chrono dependency.
fn utc_now_amz() -> String {
    let out = std::process::Command::new("date")
        .args(["-u", "+%Y%m%dT%H%M%SZ"])
        .output()
        .expect("date");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}
