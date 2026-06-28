use anyhow::Result;
use log::warn;
use spinoff::{spinners, Color, Spinner, Streams};

use crate::{
    env::update_data,
    extract::extract_variables,
    prompt::get_interaction,
    resolve::Resolved,
    scope::Scope,
    transport::{self, http::build_client},
};

pub async fn make_request(resolved: &Resolved, scope: &Scope) -> Result<()> {
    let client = build_client(&resolved.root_dir)?;

    let interaction = get_interaction(scope.clone());

    let req = transport::prepare_request(resolved, interaction)?;

    transport::print_request(&req);

    let mut spinner = Spinner::new_with_stream(
        spinners::BouncingBar,
        "",
        Color::Yellow,
        Streams::Stderr,
    );

    let response = match transport::send(&client, &req).await {
        Ok(response) => {
            spinner.stop();
            response
        }
        Err(err) => {
            spinner.stop();
            return Err(err);
        }
    };

    let result = transport::finish_response(response).await?;

    if let Some(json) = result.json {
        let vars = extract_variables(&json, scope)?;
        update_data(&resolved.root_dir, &vars)?;
    }

    warn!("# Request completed in {:.2?}", result.elapsed);

    Ok(())
}
