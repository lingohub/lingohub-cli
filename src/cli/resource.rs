// Copyright (c) 2026 lingohub GmbH and/or its affiliates.
// SPDX-License-Identifier: Apache-2.0

use super::{
    input, output,
    parse::{Cli, ResourceAction},
};
use crate::{
    error::Error,
    prompt::{self, SelectResult},
    transport::{ApiClient, RequestOptions, collection_page},
};
use reqwest::Method;

pub async fn run(client: &ApiClient, resource: &str, action: ResourceAction, cli: &Cli) -> Result<(), Error> {
    let path = format!("/v2/{resource}");
    let options = RequestOptions {
        if_match: cli.if_match.clone(),
        idempotency_key: cli.idempotency_key.clone(),
    };
    if let ResourceAction::List {
        filter,
        cursor,
        limit,
        all,
    } = action
    {
        if cli.file.is_some() || !cli.fields.is_empty() {
            return Err(Error::Input(
                "List does not accept request fields or --file".into(),
            ));
        }
        if cli.if_match.is_some() || cli.idempotency_key.is_some() {
            return Err(Error::Input(
                "List does not accept --if-match or --idempotency-key".into(),
            ));
        }
        let mut query = Vec::new();
        if let Some(filter) = filter {
            let _: serde_json::Value = serde_json::from_str(&filter)?;
            query.push(("filter".into(), filter));
        }
        if let Some(cursor) = cursor {
            query.push(("cursor".into(), cursor));
        }
        if let Some(limit) = limit {
            query.push(("limit".into(), limit.to_string()));
        }
        return output::print_value(
            &client
                .list_observed(&path, &query, all, |response| {
                    if cli.include_headers {
                        output::print_headers(response);
                    }
                })
                .await?,
            cli.json,
        );
    }
    let (method, id, needs_body) = match action {
        ResourceAction::Get { id } => (
            Method::GET,
            Some(resolve_id(client, &path, id, cli.json).await?),
            false,
        ),
        ResourceAction::Create => (Method::POST, None, true),
        ResourceAction::Update { id } => (
            Method::PATCH,
            Some(resolve_id(client, &path, id, cli.json).await?),
            true,
        ),
        ResourceAction::Delete { id } => (
            Method::DELETE,
            Some(resolve_id(client, &path, id, cli.json).await?),
            false,
        ),
        ResourceAction::List { .. } => unreachable!(),
    };
    let path = if let Some(id) = id {
        format!("{path}/{id}")
    } else {
        path
    };
    if method == Method::DELETE
        && !cli.json
        && prompt::is_interactive()
        && !prompt::confirm(&format!("Delete {path}?"))?
    {
        return Ok(());
    }
    let body = if needs_body {
        match input::request_body(cli.file.as_deref(), &cli.fields)? {
            Some(body) => Some(body),
            None if !cli.json && prompt::is_interactive() => {
                Some(input::interactive_body(resource, method == Method::POST)?)
            }
            None => {
                return Err(Error::Input(
                    "Request body required: use --file PATH or --file - for stdin".into(),
                ));
            }
        }
    } else {
        if cli.file.is_some() || !cli.fields.is_empty() {
            return Err(Error::Input(
                "This command does not accept request fields or --file".into(),
            ));
        }
        None
    };
    output::print_response(
        client
            .request(method, &path, &[], body.as_ref(), &options)
            .await?,
        cli.json,
        cli.include_headers,
    )
}
async fn resolve_id(client: &ApiClient, path: &str, id: Option<String>, json: bool) -> Result<String, Error> {
    if let Some(id) = id {
        if id.is_empty()
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            return Err(Error::Input("Invalid public identifier".into()));
        }
        return Ok(id);
    }
    if json || !prompt::is_interactive() {
        return Err(Error::Input(
            "An explicit public ID is required in non-interactive mode".into(),
        ));
    }
    select_id(client, path, prompt::select_from_list).await
}

async fn select_id(
    client: &ApiClient,
    path: &str,
    mut select: impl FnMut(&[(String, String)], usize, bool) -> std::io::Result<SelectResult>,
) -> Result<String, Error> {
    let mut query = vec![("limit".into(), "100".into())];
    let mut seen = std::collections::HashSet::new();
    loop {
        let body = client.list(path, &query, false).await?;
        let (items, cursor) = collection_page(&body)?;
        let choices: Vec<_> = items
            .iter()
            .map(|x| {
                let id = x["id"]
                    .as_str()
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| Error::Protocol("Resource has no public ID".into()))?;
                Ok((x["name"].as_str().unwrap_or("").to_owned(), id.to_owned()))
            })
            .collect::<Result<_, Error>>()?;
        if choices.is_empty() {
            return Err(Error::Input(
                "No resources available on this page; use list with an explicit cursor".into(),
            ));
        }
        match select(&choices, 0, cursor.is_some())? {
            SelectResult::Selected(i) => {
                return choices
                    .get(i)
                    .map(|c| c.1.clone())
                    .ok_or_else(|| Error::Protocol("Invalid resource selection".into()));
            }
            SelectResult::LoadMore => {
                let cursor = cursor.ok_or_else(|| Error::Protocol("No further resource page".into()))?;
                if !seen.insert(cursor.to_owned()) {
                    return Err(Error::Protocol("Server repeated a pagination cursor".into()));
                }
                query.retain(|(name, _)| name != "cursor");
                query.push(("cursor".into(), cursor.into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    #[tokio::test]
    async fn selection_fetches_another_page_only_when_requested() {
        for load_more in [false, true] {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let origin = format!("http://{}", listener.local_addr().unwrap());
            let server = std::thread::spawn(move || {
                for index in 0..if load_more { 2 } else { 1 } {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream
                        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                        .unwrap();
                    let mut request = [0; 4096];
                    let n = stream.read(&mut request).unwrap();
                    let request = String::from_utf8_lossy(&request[..n]);
                    assert_eq!(request.contains("cursor=next"), index == 1);
                    let body = serde_json::json!({"object_type":"LIST", "data":[{"id":format!("stack_{index}"),"name":"Choice"}], "has_more":index==0, "next_cursor":if index==0 {Some("next")} else {None}}).to_string();
                    write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",body.len(),body).unwrap();
                }
            });
            let client = ApiClient::new(&origin, "test").unwrap();
            let mut prompts = 0;
            let selected = select_id(&client, "/v2/stacks", |items, _, more| {
                assert_eq!(items.len(), 1);
                prompts += 1;
                Ok(if load_more && prompts == 1 {
                    assert!(more);
                    SelectResult::LoadMore
                } else {
                    SelectResult::Selected(0)
                })
            })
            .await
            .unwrap();
            assert_eq!(selected, if load_more { "stack_1" } else { "stack_0" });
            assert_eq!(prompts, if load_more { 2 } else { 1 });
            server.join().unwrap();
        }
    }
}
