// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

#![feature(drain_filter)]
#![feature(new_uninit)]
#![feature(get_mut_unchecked)]
#![feature(async_closure)]
#![feature(hash_drain_filter)]

//! The game server has authority over all game logic. Clients are served the client, which connects
//! via websocket.

use crate::protocol::Authenticate;
use actix::prelude::*;
use actix_http::header::HeaderValue;
use actix_http::KeepAlive;
use actix_web::dev::Service;
use actix_web::http::header::CACHE_CONTROL;
use actix_web::{middleware, web, App, Error, HttpRequest, HttpResponse, HttpServer};
use actix_web_actors::ws;
use actix_web_middleware_redirect_https::RedirectHTTPS;
use common::entity::EntityType;
use common::protocol::{Command, Update};
use core::admin::{AdminState, ParameterizedAdminRequest};
use core::client::ParametrizedClientRequest;
use core_protocol::dto::InvitationDto;
use core_protocol::id::*;
use core_protocol::rpc::{AdminRequest, ClientRequest, ClientUpdate};
use core_protocol::web_socket::WebSocketFormat;
use env_logger;
use futures::{pin_mut, select, FutureExt};
use log::{debug, error, info, warn, LevelFilter};
use serde::Deserialize;
use servutil::ssl::Ssl;
use servutil::web_socket::{sock_index, WebSocket};
use structopt::StructOpt;

mod arena;
mod bot;
mod collision;
mod complete_ref;
mod contact_ref;
mod entities;
mod entity;
mod entity_extension;
mod noise;
mod player;
mod protocol;
mod server;
mod world;
mod world_inbound;
mod world_mutation;
mod world_outbound;
mod world_physics;
mod world_physics_radius;
mod world_spawn;

/// Server options, to be specified as arguments.
#[derive(Debug, StructOpt)]
struct Options {
    /// Minimum player count (to be achieved by adding bots)
    #[structopt(short = "p", long, default_value = "30")]
    min_players: usize,
    /// Verbosity
    #[structopt(short, long, parse(from_occurrences))]
    verbose: usize,
    /// Log incoming HTTP requests
    #[structopt(long)]
    debug_http: bool,
    /// Log game diagnostics
    #[structopt(long)]
    debug_game: bool,
    /// Log core diagnostics
    #[structopt(long)]
    debug_core: bool,
    /// Log socket diagnostics
    #[structopt(long)]
    debug_sockets: bool,
    /// Log chats
    #[structopt(long)]
    chat_log: Option<String>,
    // Don't write to the database.
    #[structopt(long)]
    database_read_only: bool,
    // Server id.
    #[structopt(long, default_value = "0")]
    server_id: u8,
    // Domain.
    #[allow(dead_code)]
    #[structopt(long)]
    domain: Option<String>,
    // Certificate chain path
    #[structopt(long)]
    certificate_path: Option<String>,
    // Private key path
    #[structopt(long)]
    private_key_path: Option<String>,
}

#[derive(Deserialize)]
struct WebSocketFormatQuery {
    format: Option<WebSocketFormat>,
}

/// ws_index routes incoming HTTP requests to WebSocket connections.
async fn ws_index(
    r: HttpRequest,
    stream: web::Payload,
    session_id: SessionId,
    format: WebSocketFormat,
    srv: Addr<server::Server>,
) -> Result<HttpResponse, Error> {
    match srv.send(Authenticate { session_id }).await {
        Ok(response) => match response {
            Some((player_id, invitation)) => ws::start(
                WebSocket::<Command, Update, (SessionId, PlayerId, Option<InvitationDto>)>::new(
                    srv.recipient(),
                    format,
                    (session_id, player_id, invitation),
                ),
                &r,
                stream,
            ),
            None => Ok(HttpResponse::Unauthorized().body("invalid session id")),
        },
        Err(e) => Ok(HttpResponse::InternalServerError().body(e.to_string())),
    }
}

fn main() {
    // SAFETY: As per spec, only called once (before .data()) is called.
    unsafe {
        EntityType::init();
        noise::init()
    }

    let options = Options::from_args();

    let mut logger = env_logger::builder();
    logger.format_timestamp(None);
    let level = match options.verbose {
        0 => LevelFilter::Error,
        1 => LevelFilter::Warn,
        2 => LevelFilter::Info,
        3 => LevelFilter::Debug,
        _ => LevelFilter::Trace,
    };
    if options.debug_game {
        logger.filter_module(module_path!(), level);
    }
    if options.debug_core {
        logger.filter_module("core", level);
        logger.filter_module("core_protocol", level);
    }
    if options.debug_sockets {
        logger.filter_module("servutil::web_socket", level);
    }
    if options.debug_http {
        logger.filter_module("actix_web", LevelFilter::Info);
        logger.filter_module("actix_server", LevelFilter::Info);
    }
    logger.init();

    let _ = actix_web::rt::System::new().block_on(async move {
        let core = core::core::Core::start(
            core::core::Core::new(options.chat_log, options.database_read_only).await,
        );
        let srv = server::Server::start(server::Server::new(
            ServerId::new(options.server_id),
            options.min_players,
            core.to_owned(),
        ));

        let mut ssl = options
            .certificate_path
            .as_ref()
            .zip(options.private_key_path.as_ref())
            .map(|(certificate_file, private_key_file)| {
                Ssl::new(&certificate_file, &private_key_file).unwrap()
            });

        let use_ssl = ssl.is_some();

        loop {
            let iter_core = core.to_owned();
            let iter_srv = srv.to_owned();

            // If ssl exists, safe to assume whatever certificates exist are now installed.
            ssl.as_mut().map(|ssl| ssl.set_renewed());
            let immut_ssl = &ssl;

            let mut server = HttpServer::new(move || {
                // Rust let's you get away with cloning one closure deep, not all the way to a nested closure.
                let admin_clone = iter_core.to_owned();
                let core_clone = iter_core.to_owned();
                let client_code = iter_core.to_owned();
                let status_clone = iter_core.to_owned();
                let srv_clone = iter_srv.to_owned();

                let app = App::new()
                    /*
                    .wrap_fn(move |req, srv| {
                        use core_protocol::get_unix_time_now;
                        use actix_web::dev::Service;
                        // println!("{:?}", req.version());
                        use std::fs::OpenOptions;
                        if let Some(addr) = req.connection_info().remote_addr() {
                            if let Ok(mut file) = OpenOptions::new()
                                .create(true)
                                .append(true)
                                .open("/tmp/tcp-mk48.csv")
                            {
                                use std::io::Write;
                                let _ = write!(
                                    file,
                                    "{}",
                                    format!(
                                        "{},{},{:?}\n",
                                        get_unix_time_now(),
                                        addr,
                                        req.version()
                                    )
                                );
                            }
                        }
                        srv.call(req)
                    })
                     */
                    .wrap(RedirectHTTPS::default().set_enabled(use_ssl))
                    .wrap(middleware::Logger::default())
                    .service(web::resource("/client/ws/").route(web::get().to(
                        move |r: HttpRequest, stream: web::Payload| {
                            sock_index::<core::core::Core, ClientRequest, ClientUpdate>(
                                r,
                                stream,
                                core_clone.to_owned(),
                            )
                        },
                    )))
                    .service(web::resource("/ws/{session_id}/").route(web::get().to(
                        move |r: HttpRequest,
                              stream: web::Payload,
                              path: web::Path<SessionId>,
                              query: web::Query<WebSocketFormatQuery>| {
                            ws_index(
                                r,
                                stream,
                                path.into_inner(),
                                query.into_inner().format.unwrap_or_default(),
                                srv_clone.to_owned(),
                            )
                        },
                    )))
                    .service(web::resource("/client/").route(web::post().to(
                        move |request: web::Json<ParametrizedClientRequest>| {
                            let core = client_code.to_owned();
                            debug!("received client request");
                            // HttpResponse impl's Future, but that is irrelevant in this context.
                            #[allow(clippy::async_yields_async)]
                            async move {
                                match core.send(request.0).await {
                                    Ok(result) => match result {
                                        actix_web::Result::Ok(update) => {
                                            let response = serde_json::to_vec(&update).unwrap();
                                            HttpResponse::Ok().body(response)
                                        }
                                        Err(e) => HttpResponse::BadRequest().body(String::from(e)),
                                    },
                                    Err(e) => {
                                        HttpResponse::InternalServerError().body(e.to_string())
                                    }
                                }
                            }
                        },
                    )))
                    .service(web::resource("/admin/").route(web::post().to(
                        move |request: web::Json<ParameterizedAdminRequest>| {
                            let core = admin_clone.to_owned();
                            debug!("received metric request");
                            // HttpResponse impl's Future, but that is irrelevant in this context.
                            #[allow(clippy::async_yields_async)]
                            async move {
                                match core.send(request.0).await {
                                    Ok(result) => match result {
                                        actix_web::Result::Ok(update) => {
                                            let response = serde_json::to_vec(&update).unwrap();
                                            HttpResponse::Ok().body(response)
                                        }
                                        Err(e) => HttpResponse::BadRequest().body(String::from(e)),
                                    },
                                    Err(e) => {
                                        HttpResponse::InternalServerError().body(e.to_string())
                                    }
                                }
                            }
                        },
                    )))
                    .service(web::resource("/status/").route(web::get().to(move || {
                        let core = status_clone.to_owned();
                        debug!("received status request");
                        let request = ParameterizedAdminRequest {
                            params: AdminState {
                                auth: AdminState::AUTH.to_string(),
                            },
                            request: AdminRequest::RequestStatus,
                        };
                        // HttpResponse impl's Future, but that is irrelevant in this context.
                        #[allow(clippy::async_yields_async)]
                        async move {
                            match core.send(request).await {
                                Ok(result) => match result {
                                    actix_web::Result::Ok(update) => {
                                        let response = serde_json::to_vec(&update).unwrap();
                                        HttpResponse::Ok().body(response)
                                    }
                                    Err(e) => HttpResponse::BadRequest().body(String::from(e)),
                                },
                                Err(e) => HttpResponse::InternalServerError().body(e.to_string()),
                            }
                        }
                    })))
                    .wrap_fn(move |req, srv| {
                        srv.call(req).map(|mut r| {
                            if let Ok(res) = r.as_mut() {
                                res.headers_mut()
                                    .insert(CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                            }
                            r
                        })
                    });

                // Allows changing without recompilation.
                #[cfg(debug_assertions)]
                {
                    use actix_files as fs;
                    return app
                        .service(
                            fs::Files::new("/admin", "../core/js/public/").index_file("index.html"),
                        )
                        .service(fs::Files::new("/", "../js/public/").index_file("index.html"));
                }

                // Allows single-binary distribution.
                #[cfg(not(debug_assertions))]
                {
                    use actix_plus_static_files::{
                        build_hashmap_from_included_dir, include_dir, ResourceFiles,
                    };
                    app.service(ResourceFiles::new(
                        "/admin",
                        build_hashmap_from_included_dir(&include_dir!("../core/js/public/")),
                    ))
                    .service(ResourceFiles::new(
                        "/*",
                        build_hashmap_from_included_dir(&include_dir!("../js/public/")),
                    ))
                }
            });

            if let Some(ssl) = immut_ssl {
                server = server
                    .bind_rustls("0.0.0.0:443", ssl.rustls_config())
                    .expect("could not listen (https)");
                server = server.bind("0.0.0.0:80").expect("could not listen (http)");
            } else {
                server = server
                    .bind("0.0.0.0:8000")
                    .expect("could not listen (http)");
            }

            const MAX_FILE_DESCRIPTORS: usize = 1000;
            const BACKLOG: usize = 50;
            const CLEARANCE: usize = 50;
            const MAX_CONNECTIONS: usize = MAX_FILE_DESCRIPTORS - BACKLOG - CLEARANCE;
            let workers = num_cpus::get();
            let max_connections_per_worker = MAX_CONNECTIONS / workers;

            info!(
                "Server will spawn {} workers, each with up to {} connections",
                workers, max_connections_per_worker
            );

            server = server
                .keep_alive(KeepAlive::Timeout(10))
                // Don't wait forever for clients to disconnect.
                .shutdown_timeout(3)
                .max_connections(max_connections_per_worker)
                .backlog(BACKLOG as u32);

            let running_server = server.run();

            if use_ssl {
                // This clone can be sent the stop command, and it will stop the original server
                // which has been moved by then.
                let stoppable_server = running_server.clone();

                let renewal = async move {
                    let mut interval =
                        tokio::time::interval(tokio::time::Duration::from_secs(12 * 60 * 60));

                    // Eat first tick.
                    interval.tick().await;

                    loop {
                        interval.tick().await;

                        if immut_ssl.as_ref().unwrap().can_renew() {
                            warn!("Checking if certificate can be renewed...yes");
                            // Stopping this future will trigger a restart.
                            break;
                        } else {
                            info!("Checking if certificate can be renewed...no");
                        }
                    }
                };

                //let fused_server = (Box::new(running_server) as Box<dyn futures::Future<Output=Result<(), std::io::Error>>>);
                let fused_server = running_server.fuse();
                let fused_renewal = renewal.fuse();

                pin_mut!(fused_server, fused_renewal);

                select! {
                    res = fused_server => {
                        error!("server result: {:?}", res);
                        break;
                    },
                    () = fused_renewal => stoppable_server.stop(true).await
                }
            } else {
                let _ = running_server.await;
                break;
            }
        }
    });
}