use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    hitman::lsp::serve().await
}
