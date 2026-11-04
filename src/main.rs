// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

mod action;
mod api;
mod cli;
mod colors;
mod components;
mod contract;
mod error;
mod prompt;
mod transport;
mod tui_app;

use clap::{CommandFactory, FromArgMatches, parser::ValueSource};
use clap_complete::{Shell::Bash, aot::generate};
use clap_i18n_richformatter::{ClapI18nRichFormatter, init_clap_rich_formatter_localizer};
use cli::{
    login::KeyOverride,
    parse::{ApiAction, Cli, ColorMode, Commands},
};
use error::Error;
use reqwest::Method;
use transport::{ApiClient, RequestOptions};

fn main() {
    // Login and logout tell whether a key outside the user configuration is in use, so note
    // whether the process environment had one before that configuration can add its own.
    let environment_api_key = std::env::var_os(cli::login::API_KEY_VARIABLE).is_some();
    // Load only the user's trusted configuration, before runtime threads exist.
    // Working-directory .env files must not redirect a stored credential.
    if let Some(path) = cli::login::config_file() {
        dotenvy::from_path(path).ok();
    }
    cli::upgrade::remove_replaced_binary();
    run_cli(environment_api_key);
}

#[tokio::main]
async fn run_cli(environment_api_key: bool) {
    init_clap_rich_formatter_localizer();
    let matches = Cli::command()
        .try_get_matches()
        .unwrap_or_else(|err| err.apply::<ClapI18nRichFormatter>().exit());
    let mut cli = Cli::from_arg_matches(&matches)
        .map_err(|err| err.format(&mut Cli::command()))
        .unwrap_or_else(|err| err.apply::<ClapI18nRichFormatter>().exit());
    let key_override = if matches.value_source("api_key") == Some(ValueSource::CommandLine) {
        Some(KeyOverride::Flag)
    } else if environment_api_key {
        Some(KeyOverride::Environment)
    } else {
        None
    };
    colors::set_color_enabled(
        matches!(cli.color, ColorMode::Always),
        matches!(cli.color, ColorMode::Never),
    );
    let command = cli.command.take();
    let update_check = cli::upgrade::start(command.as_ref(), &cli);
    let result = run(command, &cli, key_override).await;
    if let Err(error) = &result {
        if cli.json {
            eprintln!("{}", error.as_json());
        } else {
            cli::output::print_error(error);
        }
    }
    cli::upgrade::finish(update_check).await;
    if let Err(error) = result {
        std::process::exit(error.exit_code());
    }
}

async fn run(command: Option<Commands>, cli: &Cli, key_override: Option<KeyOverride>) -> Result<(), Error> {
    let no_request = matches!(
        &command,
        None | Some(
            Commands::Completion { .. }
                | Commands::Login { .. }
                | Commands::Logout
                | Commands::Tui
                | Commands::Upgrade { .. }
                | Commands::Api {
                    action: ApiAction::Operations | ApiAction::McpTools { .. }
                }
        )
    );
    if (no_request || matches!(&command, Some(Commands::Workspace)))
        && (cli.file.is_some()
            || !cli.fields.is_empty()
            || cli.if_match.is_some()
            || cli.idempotency_key.is_some())
    {
        return Err(Error::Input(
            "This command does not accept --file, --field, --if-match or --idempotency-key".into(),
        ));
    }
    if no_request && cli.include_headers {
        return Err(Error::Input(
            "This command does not expose HTTP response headers".into(),
        ));
    }
    let command = match command {
        None => {
            Cli::command().print_help()?;
            println!();
            return Ok(());
        }
        Some(Commands::Completion { shell }) => {
            generate(
                shell.unwrap_or(Bash),
                &mut Cli::command(),
                "lingohub",
                &mut std::io::stdout(),
            );
            return Ok(());
        }
        Some(Commands::Tui) => {
            tui_app::launch_tui(cli.api_key.clone(), cli.base_url.clone())?;
            return Ok(());
        }
        Some(Commands::Upgrade { check }) => return cli::upgrade::run(check, cli).await,
        Some(Commands::Login { name, no_browser }) => {
            return cli::login::login(name, !no_browser, cli, key_override).await;
        }
        Some(Commands::Logout) => return cli::login::logout(cli, key_override).await,
        Some(Commands::Api {
            action: ApiAction::Operations,
        }) => {
            return cli::output::print_value(&contract::operations(), true);
        }
        Some(Commands::Api {
            action:
                ApiAction::McpTools {
                    all,
                    operations,
                    tags,
                },
        }) => {
            return cli::output::print_value(&cli::mcp_catalog::catalog(all, &operations, &tags)?, true);
        }
        Some(command) => command,
    };
    let client = ApiClient::new(&cli.base_url, cli.api_key.as_deref().unwrap_or(""))?;
    match command {
        Commands::Workspace => {
            if cli.file.is_some() || !cli.fields.is_empty() {
                return Err(Error::Input(
                    "Workspace does not accept request fields or --file".into(),
                ));
            }
            cli::output::print_response(
                client
                    .request(
                        Method::GET,
                        "/v2/workspace",
                        &[],
                        None,
                        &RequestOptions::default(),
                    )
                    .await?,
                cli.json,
                cli.include_headers,
            )
        }
        Commands::Stack { action } => cli::resource::run(&client, "stacks", action, cli).await,
        Commands::Project { action } => cli::resource::run(&client, "projects", action, cli).await,
        Commands::StyleGuide { action } => cli::resource::run(&client, "style-guides", action, cli).await,
        Commands::Api { action } => {
            let (method, path, query) = match action {
                ApiAction::McpServe {
                    all,
                    operations,
                    tags,
                } => {
                    if cli.file.is_some()
                        || !cli.fields.is_empty()
                        || cli.if_match.is_some()
                        || cli.idempotency_key.is_some()
                    {
                        return Err(Error::Input("MCP receives bodies and conditional headers in tool arguments; global request flags are not supported".into()));
                    }
                    return cli::mcp_server::serve(client, all, &operations, &tags).await;
                }
                ApiAction::Request { method, path, query } => (method, path, query),
                ApiAction::Call {
                    operation,
                    parameters,
                    query,
                } => {
                    let (method, path) = contract::resolve_call(
                        &operation,
                        &parameters,
                        cli.file.is_some() || !cli.fields.is_empty(),
                    )?;
                    (method, path, query)
                }
                ApiAction::Upload {
                    operation,
                    parameters,
                    query,
                    upload,
                    form,
                    form_files,
                } => {
                    let (method, path) = contract::resolve_upload(&operation, &parameters)?;
                    let multipart = cli::upload::prepare_with_files(
                        &upload,
                        &form,
                        &form_files,
                        &contract::upload_schema(&operation)?,
                    )?;
                    let options = RequestOptions {
                        if_match: cli.if_match.clone(),
                        idempotency_key: cli.idempotency_key.clone(),
                    };
                    let method = Method::from_bytes(method.as_bytes())
                        .map_err(|_| Error::Input("Invalid HTTP method".into()))?;
                    return cli::output::print_response(
                        client.upload(method, &path, &query, multipart, &options).await?,
                        cli.json,
                        cli.include_headers,
                    );
                }
                ApiAction::DownloadExport {
                    export_id,
                    output,
                    max_bytes,
                } => {
                    return cli::download::run(&client, &export_id, &output, max_bytes, cli).await;
                }
                ApiAction::DownloadExportEntry {
                    export_id,
                    entry_id,
                    output,
                    max_bytes,
                    max_pages,
                } => {
                    return cli::download::run_entry(
                        &client, &export_id, &entry_id, &output, max_bytes, max_pages, cli,
                    )
                    .await;
                }
                ApiAction::DownloadDistributionRelease {
                    distribution_release_id,
                    output,
                    max_bytes,
                } => {
                    return cli::download::run_release(
                        &client,
                        &distribution_release_id,
                        &output,
                        max_bytes,
                        cli,
                    )
                    .await;
                }
                ApiAction::Wait {
                    operation,
                    parameters,
                    interval,
                    timeout,
                } => {
                    return cli::wait::run(&client, &operation, &parameters, interval, timeout, cli).await;
                }
                ApiAction::Operations | ApiAction::McpTools { .. } => unreachable!(),
            };
            let method = Method::from_bytes(method.as_bytes())
                .map_err(|_| Error::Input("Invalid HTTP method".into()))?;
            if method == Method::GET && (cli.file.is_some() || !cli.fields.is_empty()) {
                return Err(Error::Input(
                    "GET does not accept request fields or --file".into(),
                ));
            }
            let body = cli::input::request_body(cli.file.as_deref(), &cli.fields)?;
            let options = RequestOptions {
                if_match: cli.if_match.clone(),
                idempotency_key: cli.idempotency_key.clone(),
            };
            let expected = contract::expected_response(method.as_str(), &path);
            cli::output::print_expected_response(
                client
                    .request(method, &path, &query, body.as_ref(), &options)
                    .await?,
                cli.json,
                cli.include_headers,
                expected,
                body.as_ref(),
            )
        }
        _ => unreachable!(),
    }
}
