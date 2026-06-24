use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let home = std::env::var("HOME")?;
    let dir_s = format!("{home}/.quorum/dispatch/mux");
    let dir = Path::new(&dir_s);
    let name = std::env::args().nth(1).expect("usage: wheeltest <session>");

    let before = sbmux::client::session_client::get_history_session(Some(dir), &name).await?;
    // wheel-up x5 (SGR encoding, button 64) at col 40, row 10
    let wheel: Vec<u8> = b"\x1b[<64;40;10M".repeat(5);
    sbmux::client::session_client::send_input_session(Some(dir), None, &name, wheel).await?;
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let after = sbmux::client::session_client::get_history_session(Some(dir), &name).await?;
    let changed = before != after;
    eprintln!(
        "before: {} lines, after: {} lines, changed: {changed}",
        before.len(),
        after.len()
    );
    // scroll back down to restore view
    let wheeldown: Vec<u8> = b"\x1b[<65;40;10M".repeat(5);
    sbmux::client::session_client::send_input_session(Some(dir), None, &name, wheeldown).await?;
    Ok(())
}
