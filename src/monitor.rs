use anyhow::{bail, Result};
use log::warn;
use tokio::time::sleep;

use std::time::Duration;

use crate::{
    oauth,
    prompt::get_interaction,
    resolve::Resolved,
    scope::Scope,
    transport::{self, http::build_client},
};

pub async fn monitor(
    resolved: &Resolved,
    target: &str,
    delay_seconds: i32,
    scope: &Scope,
) -> Result<()> {
    let Ok(delay) = u64::try_from(delay_seconds) else {
        bail!("Invalid delay");
    };

    let client = build_client(&resolved.root_dir)?;

    warn!("# Repeating every {delay} seconds, until interrupted...");

    loop {
        let scope = oauth::resolve_scope(resolved, target, scope).await?;
        let interaction = get_interaction(scope);

        let req = transport::prepare_request(resolved, interaction)?;
        let res = transport::send(&client, &req).await;

        match res {
            Ok(res) => {
                let ts = chrono::Utc::now();
                println!("{}, {}, {:.2?}", ts, res.status_code(), res.elapsed);
            }
            Err(e) => {
                eprintln!("{e}");
            }
        }

        sleep(Duration::from_secs(delay)).await;
    }
}
