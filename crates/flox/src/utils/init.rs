use anyhow::Ok;
use anyhow::Result;
use log::debug;
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::io::BufRead;
use std::io::BufReader;
use std::iter;
use std::path::Path;
use std::str::FromStr;
use tokio::io::AsyncWriteExt;

use flox_rust_sdk::prelude::Channel;
use flox_rust_sdk::prelude::ChannelRegistry;

const ENV_GIT_CONFIG_SYSTEM: &'static str = "GIT_CONFIG_SYSTEM";
const ENV_FLOX_ORIGINAL_GIT_CONFIG_SYSTEM: &'static str = "FLOX_ORIGINAL_GIT_CONFIG_SYSTEM";

pub fn init_channels() -> Result<ChannelRegistry> {
    let mut channels = ChannelRegistry::default();
    channels.register_channel("flox", Channel::from_str("github:flox/floxpkgs")?);
    channels.register_channel("nixpkgs", Channel::from_str("github:flox/nixpkgs/stable")?);
    channels.register_channel(
        "nixpkgs-flox",
        Channel::from_str("github:flox/nixpkgs-flox/master")?,
    );

    // generate these dynamically based on <?>
    channels.register_channel(
        "nixpkgs-stable",
        Channel::from_str("github:flox/nixpkgs/stable")?,
    );
    channels.register_channel(
        "nixpkgs-staging",
        Channel::from_str("github:flox/nixpkgs/staging")?,
    );
    channels.register_channel(
        "nixpkgs-unstable",
        Channel::from_str("github:flox/nixpkgs/unstable")?,
    );

    Ok(channels)
}

pub fn init_access_tokens(
    config_tokens: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    #[derive(Deserialize)]
    struct GhHost {
        oauth_token: String,
    }

    let gh_config_file = xdg::BaseDirectories::with_prefix("gh")?.get_config_file("hosts.yml");
    let gh_tokens: HashMap<String, String> = if gh_config_file.exists() {
        serde_yaml::from_reader::<_, HashMap<String, GhHost>>(File::open(gh_config_file)?)?
            .into_iter()
            .map(|(k, v)| (k, v.oauth_token))
            .collect()
    } else {
        Default::default()
    };

    let nix_tokens_file = xdg::BaseDirectories::with_prefix("nix")?.get_config_file("nix.conf");
    let nix_tokens: HashMap<String, String> = if nix_tokens_file.exists() {
        let mut tokens = HashMap::new();
        for line in BufReader::new(File::open(nix_tokens_file)?).lines() {
            let line = line.unwrap();
            let (k, v) = if let Some(l) = line.split_once("=") {
                l
            } else {
                continue;
            };

            match (k.trim(), v.trim()) {
                ("access-tokens", tt) | ("extra-access-tokens", tt) => {
                    tokens.extend(tt.split_ascii_whitespace().into_iter().map(|t| {
                        let (tk, tv) = t.split_once("=").unwrap();
                        (tk.to_string(), tv.to_string())
                    }));
                }
                _ => {}
            }
        }
        tokens
    } else {
        debug!("no default user nix.conf found - weird");
        Default::default()
    };

    let beta_access = [
        "github.com/flox/capacitor",
        "github.com/flox/nixpkgs-flox",
        "github.com/flox/nixpkgs-catalog",
        "github.com/flox/catalog-ingest",
        "github.com/flox/flox-extras",
    ]
    .into_iter()
    .map(String::from)
    .zip(iter::repeat(env!("BETA_ACCESS_TOKEN").to_string()));

    let mut tokens = HashMap::new();

    tokens.extend(gh_tokens.into_iter());
    tokens.extend(nix_tokens.into_iter());
    tokens.extend(config_tokens.clone().into_iter());
    tokens.extend(beta_access);

    Ok(tokens)
}

pub async fn init_git_conf(temp_dir: &Path) -> Result<()> {
    // Get the backed up `GIT_CONFIG_SYSTEM` set by a parent invocation of `flox`
    // May be empty if `GIT_CONFIG_SYSTEM` not set outside of flox.
    // If not empty is expected to point to an existing file.
    let backed_system_conf = match env::var(ENV_FLOX_ORIGINAL_GIT_CONFIG_SYSTEM) {
        Result::Ok(c) => Some(c),
        _ => None,
    };

    // `GIT_CONFIG_SYSTEM` as outside flox or by parent flox instance.
    // Ignored if absent, empty or pointing to a non-existent file.
    let current_system_conf = match env::var(ENV_GIT_CONFIG_SYSTEM) {
        Result::Ok(c) if c != "" && Path::new(&c).exists() => Some(c),
        _ => None,
    };

    // Recall or load the system config if it exists
    let system_conf = match (
        current_system_conf.as_deref(),
        backed_system_conf.as_deref(),
    ) {
        // Use `GIT_CONFIG_SYSTEM` if `FLOX_ORIGINAL_GIT_CONFIG_SYSTEM` is not set.
        // Corresponds to first/"outermost" invocation of flox.
        (Some(c), None) => Some(c),

        // No prior backed up system gitconfig
        (_, Some("")) => None,

        // If an original configuration was backed up, use that one.
        // `GIT_CONFIG_SYSTEM` would refer to the one set by a parent flox instance
        (_, Some(c)) => Some(c),

        // If no backed up config extists, use the default global config file
        _ if Path::new("/etc/gitconfig").exists() => Some("/etc/gitconfig"),

        // if neither exists, no other system config is applied
        _ => None,
    };

    // the flox specific git config
    let git_config = format!(
        include_str!("./gitConfig.in"),
        betaToken = env!("BETA_ACCESS_TOKEN"),
        original_include = system_conf
            .as_ref()
            .map(|c| format!("path = {c}"))
            .unwrap_or("; no original system git config".to_string())
    );

    // create a file in the process directory containing the git config
    let temp_system_conf_path = temp_dir.join("gitconfig");
    tokio::fs::OpenOptions::new()
        .write(true)
        .mode(0o600)
        .create_new(true)
        .open(&temp_system_conf_path)
        .await?
        .write_all(git_config.as_bytes())
        .await?;

    // Set system config variable
    env::set_var(ENV_GIT_CONFIG_SYSTEM, temp_system_conf_path);
    // Set the `FLOX_ORIGINAL_GIT_CONFIG_SYSTEM` variable.
    // This will be empty, if no system wide configuration is applied.
    // In an inner invocation the existence of this variable means that `GIT_CONFIG_SYSTEM` was
    // set by flox.
    env::set_var(
        ENV_FLOX_ORIGINAL_GIT_CONFIG_SYSTEM,
        system_conf.unwrap_or_default(),
    );

    Ok(())
}