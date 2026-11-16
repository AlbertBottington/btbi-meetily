//! Mac worker: only config/credential metadata is local, never audio.
use anyhow::{ensure, Context, Result};
use btbi_core::{
    graph,
    queue::{self, Destination},
};
use serde::{Deserialize, Serialize};
use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};
#[derive(Serialize, Deserialize)]
struct Config {
    destination: Destination,
    profile_id: Option<String>,
}
fn credential(c: &Config) -> Result<keyring::Entry> {
    Ok(keyring::Entry::new(
        "BTBI Meetily Assembly",
        &format!(
            "{}:{}:{}",
            c.destination.tenant_id,
            c.destination.client_id,
            c.profile_id.as_deref().context("Sign in first")?
        ),
    )?)
}
async fn access(c: &Config) -> Result<String> {
    let entry = credential(c)?;
    let token = graph::refresh(&c.destination, &entry.get_password()?).await?;
    if let Some(refresh) = token.refresh_token {
        entry.set_password(&refresh)?;
    }
    let profile = graph::profile(&token.access_token).await?;
    ensure!(
        Some(profile.id.as_str()) == c.profile_id.as_deref(),
        "Worker account changed"
    );
    Ok(token.access_token)
}
fn load(path: &Path) -> Result<Config> {
    Ok(serde_json::from_reader(std::fs::File::open(path)?)?)
}
async fn run(command: &str, path: PathBuf) -> Result<()> {
    match command {
        "configure" => {
            ensure!(
                !path.exists(),
                "Config already exists; review before replacement"
            );
            let mut input = String::new();
            io::stdin().take(65537).read_to_string(&mut input)?;
            ensure!(input.len() <= 65536, "Configuration too large");
            let destination: Destination = serde_json::from_str(&input)?;
            destination.validate()?;
            queue::atomic_json(
                &path,
                &Config {
                    destination,
                    profile_id: None,
                },
            )?;
        }
        "signin" => {
            let mut c = load(&path)?;
            let code = graph::begin(&c.destination).await?;
            println!(
                "{}",
                serde_json::json!({"verification_uri":code.verification_uri,"user_code":code.user_code})
            );
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(code.expires_in.min(1800));
            let mut interval = code.interval.unwrap_or(5).max(5);
            while tokio::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_secs(interval)).await;
                match graph::poll(&c.destination, &code.device_code).await {
                    Ok(Some(token)) => {
                        let p = graph::profile(&token.access_token).await?;
                        graph::Graph::new(token.access_token)?
                            .validate_destination(&c.destination)
                            .await?;
                        c.profile_id = Some(p.id);
                        credential(&c)?.set_password(
                            token
                                .refresh_token
                                .as_deref()
                                .context("No refresh credential returned")?,
                        )?;
                        queue::atomic_json(&path, &c)?;
                        println!("Signed in; credentials stored in OS keychain.");
                        return Ok(());
                    }
                    Ok(None) => {}
                    Err(e) if e.to_string() == "slow_down" => interval += 5,
                    Err(e) => return Err(e),
                }
            }
            anyhow::bail!("Sign-in expired");
        }
        "once" | "run" => {
            // Run one service instance. Overlapping manual runs can conflict;
            // immutable uploads and conditional cleanup fail closed on conflict.
            loop {
                let c = load(&path)?;
                let graph = graph::Graph::new(access(&c).await?)?;
                let ids = graph.ready_cloud_captures(&c.destination).await?;
                for id in ids {
                    let graph = graph::Graph::new(access(&c).await?)?;
                    match graph.process_cloud_capture(&c.destination, id).await {
                        Ok(_) => println!(
                            "{}",
                            serde_json::json!({"capture_id":id,"status":"verified"})
                        ),
                        Err(_) => eprintln!(
                            "{}",
                            serde_json::json!({"capture_id":id,"status":"needs_attention"})
                        ),
                    }
                }
                if command == "once" {
                    break;
                }
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
        _ => anyhow::bail!("Usage: btbi-cloud-worker configure|signin|once|run CONFIG_PATH"),
    }
    Ok(())
}
#[tokio::main]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.len() != 3 {
        eprintln!("Usage: btbi-cloud-worker configure|signin|once|run CONFIG_PATH");
        std::process::exit(2);
    }
    if let Err(e) = run(&args[1], PathBuf::from(&args[2])).await {
        // Network errors in graph module are redacted; never print credentials.
        eprintln!("Worker stopped: {}", e);
        std::process::exit(1);
    }
}
