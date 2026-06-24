use std::path::Path;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let name = std::env::args()
        .nth(1)
        .expect("usage: history_probe <session>");
    let home = std::env::var("HOME")?;
    let dir = format!("{home}/.quorum/dispatch/mux");
    let lines =
        sbmux::client::session_client::get_history_session(Some(Path::new(&dir)), &name).await?;
    eprintln!("total lines (scrollback + visible): {}", lines.len());
    for (i, l) in lines.iter().take(5).enumerate() {
        eprintln!(
            "  [{}] {:?}",
            i,
            String::from_utf8_lossy(l)
                .chars()
                .take(80)
                .collect::<String>()
        );
    }
    Ok(())
}
