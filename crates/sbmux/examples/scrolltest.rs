use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let home = std::env::var("HOME")?;
    let dir_s = "/tmp/sbmux-probe".to_string();
    std::fs::create_dir_all(&dir_s)?;
    let dir = Path::new(&dir_s);
    let name = "scrolltest-probe";
    let launch = sbmux::client::server_launcher::ServerLaunchSpec {
        program: format!("{home}/.bun/bin/sb").into(),
        args_prefix: vec!["sbmux-server".to_string()],
    };
    let acked = sbmux::client::session_client::create_detached_session(
        Some(dir),
        Some(&launch),
        name,
        "seq 1 200; sleep 300",
        std::path::PathBuf::from(&home),
        10_000,
    )
    .await?;
    eprintln!("created: {acked}");
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    let lines = sbmux::client::session_client::get_history_session(Some(dir), name).await?;
    eprintln!("history+visible lines: {}", lines.len());
    if let Some(first) = lines.first() {
        eprintln!("first: {:?}", String::from_utf8_lossy(first));
    }
    if let Some(last) = lines.last() {
        eprintln!("last: {:?}", String::from_utf8_lossy(last));
    }
    sbmux::client::session_client::kill_session_session(Some(dir), name).await?;
    eprintln!("killed");
    Ok(())
}
