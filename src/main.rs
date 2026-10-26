mod agent;
mod github;
mod server;
mod store;

#[cfg(test)]
mod tests;

use std::{env, sync::Arc, time::Duration};

use anyhow::{Context, Result, bail};
use goose_provider_types::{base::Provider, model::ModelConfig};
use goose_providers::{
    api_client::{ApiClient, AuthMethod},
    openai::OpenAiProvider,
};
use tokio::sync::Notify;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ngrc_server=info".into()),
        )
        .init();
    let mode = env::var("APP_MODE").unwrap_or_else(|_| "demo".into());
    let demo = match mode.as_str() {
        "demo" => true,
        "live" => false,
        _ => bail!("APP_MODE must be demo or live"),
    };
    let required = |name| {
        env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
            .with_context(|| format!("{name} is required in live mode"))
    };
    let (provider, model): (Arc<dyn Provider>, ModelConfig) = if demo {
        (Arc::new(agent::DemoProvider), ModelConfig::new("demo"))
    } else {
        let client = ApiClient::with_timeout_and_tls(
            "https://api.openai.com".into(),
            AuthMethod::BearerToken(required("OPENAI_API_KEY")?),
            Duration::from_secs(60),
            Some(Default::default()),
        )?;
        (
            Arc::new(OpenAiProvider::new(client)),
            ModelConfig::new(required("AGENT_MODEL")?),
        )
    };
    let repository = if demo {
        "acme/widgets".into()
    } else {
        required("GITHUB_REPOSITORY")?
    };
    if !github::valid_repository(&repository) {
        bail!("GITHUB_REPOSITORY must be owner/repo");
    }
    let app = Arc::new(server::App {
        store: store::Store::open(
            env::var("DATABASE_PATH").unwrap_or_else(|_| "ci-agent.sqlite3".into()),
        )?,
        github: github::GitHub::new(if demo {
            None
        } else {
            Some(required("GITHUB_TOKEN")?)
        })?,
        provider,
        model,
        repository,
        webhook_secret: if demo {
            None
        } else {
            Some(required("GITHUB_WEBHOOK_SECRET")?)
        },
        admin_token: if demo {
            None
        } else {
            Some(required("ADMIN_TOKEN")?)
        },
        demo,
        wake: Notify::new(),
    });
    server::serve(
        app,
        env::var("BIND_ADDR")
            .unwrap_or_else(|_| "127.0.0.1:3000".into())
            .parse()?,
    )
    .await
}
