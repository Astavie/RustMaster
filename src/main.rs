#![allow(incomplete_features)]
#![feature(async_fn_in_trait)]

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use dotenv_codegen::dotenv;
use futures_util::StreamExt;
use isahc::{http::StatusCode, AsyncReadResponseExt};
use serde::{de::DeserializeOwned, Deserialize};
use tokio::sync::Mutex;

use crate::discord::application::{Application, ApplicationResource};
use crate::discord::command::CommandData;
use crate::discord::command::Commands;
use crate::discord::gateway::Gateway;
use crate::discord::request::{Client, Request, RequestError, Result};
use crate::discord::resource::Deletable;
use crate::discord::{
    gateway::GatewayEvent,
    interaction::{Interaction, InteractionData, InteractionResource},
};

mod discord;

struct DiscordRateLimits {
    request_rate: f32,
    last_request: Instant,
    retry_after: Instant,

    buckets: HashMap<String, RateLimit>,
    bucket_cache: HashMap<String, String>,
}

#[derive(Clone)]
struct Discord {
    token: String,
    limits: Arc<Mutex<DiscordRateLimits>>,
}

struct RateLimit {
    remaining: u64,
    reset_at: Instant,
}

#[derive(Deserialize)]
struct RateLimitResponse {
    retry_after: f64,
}

const GLOBAL_RATE_LIMIT: f32 = 45.0;

impl DiscordRateLimits {
    fn inc_request(&mut self) {
        let now = Instant::now();
        let diff = now.duration_since(self.last_request).as_secs_f32();
        self.request_rate = (self.request_rate + 1.0) / (diff + 1.0);
        self.last_request = now;
    }
}

impl Discord {
    fn new<S: Into<String>>(token: S) -> Self {
        Self {
            token: token.into(),
            limits: Arc::new(Mutex::new(DiscordRateLimits {
                request_rate: 0.0,
                last_request: Instant::now(),
                retry_after: Instant::now(),

                buckets: HashMap::new(),
                bucket_cache: HashMap::new(),
            })),
        }
    }
    fn get_bucket(uri: &str) -> String {
        if uri.starts_with("/guilds/") || uri.starts_with("/channels/") {
            let s: String = uri.split_inclusive('/').take(3).collect();
            s.strip_suffix("/").unwrap_or(&s).to_owned()
        } else {
            uri.to_owned()
        }
    }
    fn bound_to_global_limit(uri: &str) -> bool {
        if uri.starts_with("/interactions/") || uri.starts_with("/webhooks/") {
            false
        } else {
            true
        }
    }
}

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

const RUSTMASTER: &str = dotenv!("RUSTMASTER");
const CARDMASTER: &str = dotenv!("CARDMASTER");

impl Client for Discord {
    async fn request_weak<T: DeserializeOwned + Unpin>(&self, request: &Request<T>) -> Result<T> {
        let bucket = Discord::get_bucket(&request.uri);

        // rate limits
        let now = {
            let mut me = self.limits.lock().await;
            let now = Instant::now();

            let mut time = me.retry_after.duration_since(now);

            // global rate limit
            let global = Discord::bound_to_global_limit(&request.uri);
            if global && me.request_rate >= GLOBAL_RATE_LIMIT {
                time = time.max(Duration::from_secs_f32(1.0 / GLOBAL_RATE_LIMIT));
            }

            // local rate limit
            if let Some(bucket_id) = me.bucket_cache.get(&bucket) {
                let limit = &me.buckets[bucket_id];

                // check bucket remaining
                if limit.remaining == 0 {
                    time = time.max(limit.reset_at.duration_since(now));
                }
            }

            // sleep
            if !time.is_zero() {
                tokio::time::sleep(time).await;
            }

            if global {
                me.inc_request();
            }

            Instant::now()
        };

        // send request
        let http = isahc::Request::builder()
            .method(request.method.clone())
            .uri("https://discord.com/api/v10".to_owned() + &request.uri)
            .header(
                "User-Agent",
                format!("DiscordBot ({}, {})", "https://astavie.github.io/", VERSION),
            )
            .header("Authorization", "Bot ".to_owned() + &self.token);

        let mut response = if let Some(body) = request.body.as_ref() {
            let request = http
                .header("Content-Type", "application/json")
                .body(body.clone())
                .unwrap();
            isahc::send_async(request)
        } else {
            let request = http.body(()).unwrap();
            isahc::send_async(request)
        }
        .await
        .map_err(|err| {
            if err.is_client() || err.is_server() || err.is_tls() {
                RequestError::Authorization
            } else {
                RequestError::Network
            }
        })?;

        // update rate limit
        if let Some(remaining) = response.headers().get("X-RateLimit-Remaining") {
            let remaining = remaining.to_str().unwrap();
            let remaining: u64 = remaining.parse().unwrap();

            if let Some(reset_after) = response.headers().get("X-RateLimit-Reset-After") {
                let reset_after = reset_after.to_str().unwrap();
                let reset_after: f64 = reset_after.parse().unwrap();

                if let Some(bucket_id) = response.headers().get("X-RateLimit-Bucket") {
                    let bucket_id = bucket_id.to_str().unwrap();
                    let reset_at = now + Duration::from_secs_f64(reset_after);
                    let limit = RateLimit {
                        remaining,
                        reset_at,
                    };

                    let mut me = self.limits.lock().await;
                    me.bucket_cache.insert(bucket, bucket_id.to_owned());
                    me.buckets.insert(bucket_id.to_owned(), limit);
                }
            }
        }

        // check errors
        if response.status() == StatusCode::TOO_MANY_REQUESTS {
            // check for global limit
            if let Some(scope) = response.headers().get("X-RateLimit-Scope") {
                if scope == "global" {
                    let response: RateLimitResponse = response
                        .json()
                        .await
                        .expect("429 response contains expected json body");

                    let mut me = self.limits.lock().await;
                    me.retry_after = now + Duration::from_secs_f64(response.retry_after);
                }
            }

            return Err(RequestError::RateLimited);
        }

        let string = response.text().await.unwrap();
        // println!("{}", string);

        if response.status().is_client_error() {
            return Err(RequestError::ClientError(response.status()));
        }

        if response.status().is_server_error() {
            return Err(RequestError::ServerError);
        }

        if response.status() == StatusCode::NO_CONTENT {
            Ok(serde_json::from_str("null").unwrap())
        } else {
            serde_json::from_str(&string).map_err(|_| RequestError::ServerError)
        }
    }

    async fn token(&self) -> &str {
        &self.token
    }
}

async fn purge(commands: Commands, client: &impl Client) -> Result<()> {
    for command in commands.all(client).await? {
        command.delete(client).await?;
    }
    Ok(())
}

async fn on_command(client: &impl Client, i: Interaction) -> Result<()> {
    match i.data {
        InteractionData::ApplicationCommand(command) => match command.name.as_str() {
            "ping" => {
                i.token
                    .reply(client, |m| m.content("hurb".to_owned()))
                    .await?
            }
            _ => {}
        },
        InteractionData::MessageComponent(_) => todo!(),
    };
    Ok(())
}

async fn run() -> Result<()> {
    let client = Discord::new(RUSTMASTER);

    let application = Application::get(&client).await?;
    purge(application.global_commands(), &client).await?;
    application
        .global_commands()
        .create(
            &client,
            &CommandData::builder()
                .name("ping".to_owned())
                .description("Replies with pong!".to_owned())
                .build()
                .unwrap(),
        )
        .await?;

    let mut gateway = Gateway::connect(&client).await?;

    while let Some(event) = gateway.next().await {
        match event {
            GatewayEvent::InteractionCreate(i) => on_command(&client, i).await?,
            _ => {}
        }
    }

    gateway.close().await;

    Ok(())
}

#[tokio::main]
async fn main() {
    run().await.unwrap()
}
