//! Prompt setup edits the lossless draft and persists only on final confirmation.
//! API failures remain interactive; configuration and persistence failures exit.

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use url::Url;

use super::{PromptIo, SetupError, SetupOutcome};
use crate::api::{ApiClient, ApiOptions};
use crate::config::{
    ConfigDocument, ConfigError, ConfigStore, DEFAULT_SPONSORBLOCK_API_URL, SaveOutcome,
};
use crate::http::Endpoints;

pub async fn run(
    store: &ConfigStore,
    document: &mut ConfigDocument,
    endpoints: Endpoints,
    prompt: &mut PromptIo,
    shutdown: CancellationToken,
    http_tracing: bool,
) -> Result<SetupOutcome, SetupError> {
    prompt
        .write(
            "Welcome to the isponsorblocktvrust cli setup wizard\n",
            &shutdown,
        )
        .await?;
    let use_proxy = yes_no(
        prompt,
        &shutdown,
        "Do you want to use system-wide proxy? (y/N) ",
        false,
    )
    .await?;
    document
        .0
        .insert("use_proxy".into(), Value::Bool(use_proxy));

    if document.0.contains_key("atvs") {
        prompt.write("The legacy atvs entry prevents service startup. Migration information:\nhttps://github.com/dmunozv04/iSponsorBlockTV/wiki/Migrate-from-V1-to-V2\n", &shutdown).await?;
        if yes_no(
            prompt,
            &shutdown,
            "Remove the legacy 'atvs' entry from this draft? (y/N) ",
            false,
        )
        .await?
        {
            document.0.remove("atvs");
        }
    }

    let mut api = api_for(document, endpoints.clone(), http_tracing)?;
    loop {
        let count = array(document, "devices")?.len();
        if !yes_no(
            prompt,
            &shutdown,
            &format!("Paired with {count} Device(s). Add more? (y/N) "),
            false,
        )
        .await?
        {
            break;
        }
        let code = loop {
            let input = prompt
                .read_line(
                    "Enter pairing code (Settings - Link with TV code): ",
                    &shutdown,
                )
                .await?;
            let normalized: String = input
                .chars()
                .filter(|character| !matches!(character, ' ' | '-'))
                .collect();
            if normalized.len() == 12 && normalized.bytes().all(|byte| byte.is_ascii_digit()) {
                break normalized;
            }
            prompt
                .write(
                    "Enter exactly twelve ASCII digits; spaces and hyphens are allowed.\n",
                    &shutdown,
                )
                .await?;
        };
        prompt.write("Pairing...\n", &shutdown).await?;
        let result = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return Err(SetupError::Interrupted),
            result = api.pair_with_code(&code) => result,
        };
        match result {
            Ok(Some(device)) => {
                let name = device.name.as_deref().unwrap_or("Unnamed device");
                prompt
                    .write(
                        &format!("Paired device: {}\n", terminal_text(name)),
                        &shutdown,
                    )
                    .await?;
                append(
                    document,
                    "devices",
                    json!({"screen_id": device.screen_id, "name": device.name, "offset": 0}),
                )?;
            }
            Ok(None) => prompt
                .write(
                    "Code rejected. Check the TV code or restart the YouTube app and try again.\n",
                    &shutdown,
                )
                .await?,
            Err(error) => {
                prompt
                    .write(
                        &format!("Pairing failed: {error}. Check connectivity and try again.\n"),
                        &shutdown,
                    )
                    .await?
            }
        }
    }

    let has_key = !string(document, "apikey")?.is_empty();
    let key_prompt = if has_key {
        "API key already specified. Change it? (y/N) "
    } else {
        "API key is only needed for the channel whitelist. Add it? (y/N) "
    };
    if yes_no(prompt, &shutdown, key_prompt, false).await? {
        prompt.write("Get a YouTube API key: https://developers.google.com/youtube/registering_an_application\n", &shutdown).await?;
        let key = prompt.read_line("Enter your API key: ", &shutdown).await?;
        document
            .0
            .insert("apikey".into(), Value::String(key.trim().to_owned()));
        // Replacing the client also discards key-scoped caches.
        api = api_for(document, endpoints, http_tracing)?;
    }

    let categories_specified = document
        .0
        .get("skip_categories")
        .is_some_and(|value| !value.is_null());
    if !categories_specified
        || yes_no(
            prompt,
            &shutdown,
            "Skip categories already specified. Change them? (y/N) ",
            false,
        )
        .await?
    {
        let categories = prompt.read_line("Enter skip categories (space or comma separated; blank disables skipping):\nsponsor, selfpromo, intro, outro, music_offtopic, interaction, exclusive_access, preview, filler, hook\n> ", &shutdown).await?;
        let values: Vec<Value> = categories
            .split(|character: char| character == ',' || character.is_whitespace())
            .filter(|value| !value.is_empty())
            .map(|value| Value::String(value.to_owned()))
            .collect();
        document
            .0
            .insert("skip_categories".into(), Value::Array(values));
    }

    if yes_no(
        prompt,
        &shutdown,
        "Do you want to whitelist channels from SponsorBlock skipping? (y/N) ",
        false,
    )
    .await?
    {
        let has_key = !string(document, "apikey")?.is_empty();
        if !has_key {
            prompt.write("A YouTube API key is required for search and service startup with a whitelist. You may still save a custom channel in an incomplete draft.\n", &shutdown).await?;
        }
        loop {
            let query = prompt
                .read_line("Enter a channel name or /exit to finish: ", &shutdown)
                .await?;
            if query.trim() == "/exit" {
                break;
            }
            let results = if has_key {
                let result = tokio::select! {
                    biased;
                    _ = shutdown.cancelled() => return Err(SetupError::Interrupted),
                    result = api.search_channels(query.trim()) => result,
                };
                match result {
                    Ok(results) => results,
                    Err(error) => {
                        prompt.write(&format!("Channel search failed: {error}. Check the key, API access and connectivity; retry or enter a custom channel.\n"), &shutdown).await?;
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            if results.is_empty() {
                prompt
                    .write(
                        "No channels found. Custom entry remains available.\n",
                        &shutdown,
                    )
                    .await?;
            }
            for (index, channel) in results.iter().take(5).enumerate() {
                prompt
                    .write(
                        &format!(
                            "{index}: {} - Subs: {}\n",
                            terminal_text(&channel.name),
                            terminal_text(&channel.subscribers)
                        ),
                        &shutdown,
                    )
                    .await?;
            }
            prompt
                .write("5: Enter a custom channel ID\n6: Go back\n", &shutdown)
                .await?;
            let selection = loop {
                let input = prompt
                    .read_line("Select an option [0-6]: ", &shutdown)
                    .await?;
                if input.trim() == "/exit" {
                    break None;
                }
                if let Ok(index) = input.trim().parse::<usize>()
                    && (index == 5 || index == 6 || (index < 5 && index < results.len()))
                {
                    break Some(index);
                }
                prompt
                    .write(
                        "Invalid selection. Choose a displayed result, 5 or 6.\n",
                        &shutdown,
                    )
                    .await?;
            };
            match selection {
                None => break,
                Some(6) => continue,
                Some(5) => {
                    let id = loop {
                        let id = prompt.read_line("Enter a channel ID: ", &shutdown).await?;
                        if !id.trim().is_empty() {
                            break id.trim().to_owned();
                        }
                        prompt
                            .write("Channel ID cannot be empty.\n", &shutdown)
                            .await?;
                    };
                    let name = prompt
                        .read_line("Enter the channel name (blank uses ID): ", &shutdown)
                        .await?;
                    let name = if name.trim().is_empty() {
                        id.clone()
                    } else {
                        name
                    };
                    append(
                        document,
                        "channel_whitelist",
                        json!({"id": id, "name": name}),
                    )?;
                }
                Some(index) => {
                    let channel = &results[index];
                    append(
                        document,
                        "channel_whitelist",
                        json!({"id": channel.id, "name": channel.name}),
                    )?;
                }
            }
        }
    }

    if yes_no(
        prompt,
        &shutdown,
        "Specify a minimum segment length to skip? (y/N) ",
        false,
    )
    .await?
    {
        loop {
            let input = prompt
                .read_line(
                    "Minimum skip length in seconds (0 disables the minimum): ",
                    &shutdown,
                )
                .await?;
            if let Some(number) = finite_number(input.trim()) {
                document.0.insert("minimum_skip_length".into(), number);
                break;
            }
            prompt
                .write(
                    "Enter a finite number of seconds within the runtime numeric range.\n",
                    &shutdown,
                )
                .await?;
        }
    }
    for (key, text, default) in [
        (
            "skip_count_tracking",
            "Report skipped segments to SponsorBlock (only segment UUIDs are sent)? (Y/n) ",
            true,
        ),
        (
            "mute_ads",
            "Mute native YouTube ads automatically? (y/N) ",
            false,
        ),
        (
            "skip_ads",
            "Skip native YouTube ads automatically? (y/N) ",
            false,
        ),
        ("auto_play", "Enable autoplay? (Y/n) ", true),
    ] {
        let value = yes_no(prompt, &shutdown, text, default).await?;
        document.0.insert(key.into(), Value::Bool(value));
    }
    loop {
        let input = prompt
            .read_line(
                &format!("SponsorBlock API URL (blank selects {DEFAULT_SPONSORBLOCK_API_URL}): "),
                &shutdown,
            )
            .await?;
        let value = if input.trim().is_empty() {
            DEFAULT_SPONSORBLOCK_API_URL
        } else {
            input.trim()
        };
        if valid_base_url(value) {
            document
                .0
                .insert("sponsorblock_api_url".into(), Value::String(value.into()));
            break;
        }
        prompt
            .write(
                "Enter an HTTP(S) URL without credentials, query or fragment.\n",
                &shutdown,
            )
            .await?;
    }
    if yes_no(prompt, &shutdown, "Save this configuration? (y/N) ", false).await? {
        if shutdown.is_cancelled() {
            return Err(SetupError::Interrupted);
        }
        // Incomplete drafts are intentionally saveable, without runtime_config.
        let outcome = store.save(document)?;
        let action = match outcome {
            SaveOutcome::Saved => "Config saved to",
            SaveOutcome::Unchanged => "Config unchanged at",
        };
        prompt
            .write(
                &format!("{action} {}\n", store.config_file().display()),
                &shutdown,
            )
            .await?;
        Ok(SetupOutcome::Saved(outcome))
    } else {
        prompt
            .write(
                &format!(
                    "No config file was saved to {}\n",
                    store.config_file().display()
                ),
                &shutdown,
            )
            .await?;
        Ok(SetupOutcome::NotSaved)
    }
}

async fn yes_no(
    prompt: &mut PromptIo,
    shutdown: &CancellationToken,
    text: &str,
    default: bool,
) -> Result<bool, SetupError> {
    loop {
        let input = prompt.read_line(text, shutdown).await?;
        match input.trim().to_ascii_lowercase().as_str() {
            "" => return Ok(default),
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => {
                prompt
                    .write("Invalid input. Enter y or n.\n", shutdown)
                    .await?
            }
        }
    }
}

fn array<'a>(document: &'a ConfigDocument, key: &str) -> Result<&'a [Value], SetupError> {
    match document.0.get(key) {
        None => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        Some(_) => Err(ConfigError::Field {
            path: key.into(),
            reason: "expected an array",
        }
        .into()),
    }
}

fn append(document: &mut ConfigDocument, key: &str, value: Value) -> Result<(), SetupError> {
    array(document, key)?;
    document
        .0
        .entry(key.to_owned())
        .or_insert_with(|| Value::Array(Vec::new()))
        .as_array_mut()
        .expect("validated array")
        .push(value);
    Ok(())
}

fn string<'a>(document: &'a ConfigDocument, key: &str) -> Result<&'a str, SetupError> {
    match document.0.get(key) {
        None => Ok(""),
        Some(Value::String(value)) => Ok(value),
        Some(_) => Err(ConfigError::Field {
            path: key.into(),
            reason: "expected a string",
        }
        .into()),
    }
}

pub(super) fn api_settings(document: &ConfigDocument) -> Result<(&str, bool), SetupError> {
    Ok((
        string(document, "apikey")?,
        document
            .0
            .get("use_proxy")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    ))
}

pub(super) fn api_for(
    document: &ConfigDocument,
    endpoints: Endpoints,
    http_tracing: bool,
) -> Result<ApiClient, SetupError> {
    let (apikey, use_proxy) = api_settings(document)?;
    // Setup performs no whitelist decisions. Excluding incomplete draft records
    // here allows editing/saving them without imposing service-start validation.
    Ok(ApiClient::new(
        ApiOptions {
            apikey: apikey.to_owned(),
            use_proxy,
            http_tracing,
            channel_whitelist: Vec::new(),
        },
        endpoints,
    )?)
}

pub(super) fn finite_number(input: &str) -> Option<Value> {
    let parsed = input
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())?;
    if parsed == 0.0
        && input
            .split(['e', 'E'])
            .next()?
            .bytes()
            .any(|byte| matches!(byte, b'1'..=b'9'))
    {
        return None;
    }
    // Preserve entered JSON numeric precision, while also accepting normal
    // prompt notation such as +1 and .5 that is not a JSON number spelling.
    Some(
        serde_json::from_str::<Value>(input)
            .ok()
            .filter(Value::is_number)
            .unwrap_or_else(|| json!(parsed)),
    )
}

pub(super) fn valid_base_url(value: &str) -> bool {
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.has_host()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn terminal_text(value: &str) -> String {
    value
        .chars()
        .filter(|character| !character.is_control())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timing_input_preserves_signed_fractions_and_rejects_unrepresentable_values() {
        assert_eq!(finite_number("-250.5"), Some(json!(-250.5)));
        assert_eq!(finite_number(".5"), Some(json!(0.5)));
        assert_eq!(finite_number("+1"), Some(json!(1.0)));
        for invalid in ["NaN", "inf", "-inf", "1e999", "1e-999", "not a number"] {
            assert!(finite_number(invalid).is_none(), "{invalid}");
        }
    }

    #[test]
    fn api_base_input_disallows_credentials_and_query_state() {
        assert!(valid_base_url("https://sponsor.ajay.app/api/"));
        assert!(valid_base_url("http://localhost:8080/custom"));
        for invalid in [
            "ftp://example.test/api/",
            "https://user:password@example.test/",
            "https://example.test/?token=secret",
            "https://example.test/#fragment",
            "/relative",
        ] {
            assert!(!valid_base_url(invalid));
        }
    }

    #[test]
    fn remote_names_cannot_inject_terminal_controls() {
        assert_eq!(terminal_text("Café 日本語"), "Café 日本語");
        assert_eq!(terminal_text("name\u{1b}[2J\r\nnext"), "name[2Jnext");
    }
}
