// SPDX-FileCopyrightText: 2021 Softbear, Inc.
// SPDX-License-Identifier: AGPL-3.0-or-later

use crate::chat::{ChatRepo, ClientChatData};
use crate::game_service::GameArenaService;
use crate::infrastructure::Infrastructure;
use crate::invitation::{ClientInvitationData, InvitationRepo};
use crate::leaderboard::LeaderboardRepo;
use crate::liveboard::LiveboardRepo;
use crate::metric::{ClientMetricData, MetricRepo};
use crate::player::{PlayerData, PlayerRepo, PlayerTuple};
use crate::system::SystemRepo;
use crate::team::{ClientTeamData, TeamRepo};
use crate::unwrap_or_return;
use actix::WrapStream;
use actix::{
    fut, ActorFutureExt, ActorStreamExt, Context as ActorContext, ContextFutureSpawner, Handler,
    Message, ResponseActFuture, WrapFuture,
};
use atomic_refcell::AtomicRefCell;
use common_util::ticks::Ticks;
use core_protocol::dto::{InvitationDto, ServerDto};
use core_protocol::get_unix_time_now;
use core_protocol::id::{ArenaId, InvitationId, PlayerId, ServerId, SessionId, UserAgentId};
use core_protocol::name::{PlayerAlias, Referrer};
use core_protocol::rpc::{
    ClientRequest, ClientUpdate, LeaderboardUpdate, LiveboardUpdate, PlayerUpdate, Request,
    SystemUpdate, TeamUpdate, Update,
};
use futures::stream::FuturesUnordered;
use log::{error, info, warn};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use server_util::benchmark::{benchmark_scope, Timer};
use server_util::database_schema::SessionItem;
use server_util::generate_id::{generate_id, generate_id_64};
use server_util::ip_rate_limiter::IpRateLimiter;
use server_util::observer::{ObserverMessage, ObserverUpdate};
use server_util::rate_limiter::{RateLimiter, RateLimiterProps};
use std::collections::hash_map::Entry;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::hash::Hash;
use std::io::Write;
use std::marker::PhantomData;
use std::net::IpAddr;
use std::ops::Deref;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::UnboundedSender;

/// The message recipient of an actix actor corresponding to a client.
pub type ClientAddr<G> =
    UnboundedSender<ObserverUpdate<Update<<G as GameArenaService>::ClientUpdate>>>;

/// Keeps track of clients a.k.a. real players a.k.a. websockets.
pub struct ClientRepo<G: GameArenaService> {
    authenticate_rate_limiter: IpRateLimiter,
    database_rate_limiter: RateLimiter,
    /// Where to log traces to.
    trace_log: Option<String>,
    _spooky: PhantomData<G>,
}

impl<G: GameArenaService> ClientRepo<G> {
    pub fn new(trace_log: Option<String>, authenticate: RateLimiterProps) -> Self {
        Self {
            authenticate_rate_limiter: authenticate.into(),
            database_rate_limiter: RateLimiter::new(Duration::from_secs(30), 0),
            trace_log,
            _spooky: PhantomData,
        }
    }

    /// Updates sessions to database (internally rate-limited).
    pub fn update_to_database(
        infrastructure: &mut Infrastructure<G>,
        ctx: &mut ActorContext<Infrastructure<G>>,
    ) {
        if infrastructure
            .context_service
            .context
            .clients
            .database_rate_limiter
            .should_limit_rate()
        {
            return;
        }

        // Mocker server id if read only, so we can still proceed.
        let server_id = unwrap_or_return!(infrastructure.server_id.or(infrastructure
            .database_read_only
            .then_some(ServerId::new(200).unwrap())));

        let queue = FuturesUnordered::new();

        for mut player in infrastructure
            .context_service
            .context
            .players
            .iter_borrow_mut()
        {
            let player_id = player.player_id;
            if let Some(client) = player.client_mut() {
                let session_item = SessionItem {
                    alias: client.alias,
                    arena_id: infrastructure.context_service.context.arena_id,
                    date_created: client.metrics.date_created,
                    date_previous: client.metrics.date_previous,
                    date_renewed: client.metrics.date_renewed,
                    date_terminated: None,
                    game_id: G::GAME_ID,
                    player_id,
                    plays: client.metrics.plays + client.metrics.previous_plays,
                    previous_id: client.metrics.session_id_previous,
                    referrer: client.metrics.referrer,
                    user_agent_id: client.metrics.user_agent_id,
                    server_id,
                    session_id: client.session_id,
                };

                if client.session_item.as_ref() != Some(&session_item) {
                    client.session_item = Some(session_item.clone());
                    if infrastructure.database_read_only {
                        warn!(
                            "would have written session item {:?} but was inhibited",
                            session_item
                        );
                    } else {
                        let database = infrastructure.database;
                        queue.push(database.put_session(session_item))
                    }
                }
            }
        }

        queue
            .into_actor(infrastructure)
            .map(|result, _, _| {
                if let Err(e) = result {
                    error!("error putting session: {:?}", e);
                }
            })
            .finish()
            .spawn(ctx);
    }

    /// Client websocket (re)connected.
    pub fn register(
        &mut self,
        player_id: PlayerId,
        register_observer: ClientAddr<G>,
        players: &mut PlayerRepo<G>,
        teams: &mut TeamRepo<G>,
        chat: &ChatRepo<G>,
        leaderboards: &LeaderboardRepo<G>,
        liveboard: &LiveboardRepo<G>,
        system: Option<&SystemRepo<G>>,
        arena_id: ArenaId,
        server_id: Option<ServerId>,
        game: &mut G,
    ) {
        let player_tuple = match players.get(player_id) {
            Some(player_tuple) => player_tuple,
            None => {
                warn!("client gone in register");
                return;
            }
        };

        let mut player = player_tuple.borrow_player_mut();

        let client = match player.client_mut() {
            Some(client) => client,
            None => {
                warn!("register wasn't a client");
                return;
            }
        };

        // Welcome the client in.
        let _ = register_observer.send(ObserverUpdate::Send {
            message: Update::Client(ClientUpdate::SessionCreated {
                arena_id,
                server_id,
                session_id: client.session_id,
                player_id,
            }),
        });

        // Don't assume client remembered anything, although it may/should have.
        *client.data.borrow_mut() = G::ClientData::default();
        client.chat.forget_state();
        client.team.forget_state();

        // Change status to connected.
        let new_status = ClientStatus::Connected {
            observer: register_observer.clone(),
        };
        let old_status = std::mem::replace(&mut client.status, new_status);

        drop(player);

        match old_status {
            ClientStatus::Connected { observer } => {
                // If it still exists, old client is now retired.
                let _ = observer.send(ObserverUpdate::Close);
            }
            ClientStatus::Limbo { .. } => {
                info!("player {:?} restored from limbo", player_id);
            }
            ClientStatus::Pending { .. } | ClientStatus::Stale { .. } => {
                // We previously left the game, so now we have to rejoin.
                game.player_joined(player_tuple);
            }
        }

        // Send initial data.
        for initializer in leaderboards.initializers() {
            let _ = register_observer.send(ObserverUpdate::Send {
                message: Update::Leaderboard(initializer),
            });
        }

        let _ = register_observer.send(ObserverUpdate::Send {
            message: Update::Liveboard(liveboard.initializer()),
        });

        chat.initialize_client(player_id, players);

        let _ = register_observer.send(ObserverUpdate::Send {
            message: Update::Player(players.initializer()),
        });

        if let Some(initializer) = teams.initializer() {
            let _ = register_observer.send(ObserverUpdate::Send {
                message: Update::Team(initializer),
            });
        }

        if let Some(system) = system {
            if let Some(initializer) = system.initializer() {
                let _ = register_observer.send(ObserverUpdate::Send {
                    message: Update::System(initializer),
                });
            }
        }
    }

    /// Client websocket disconnected.
    pub fn unregister(
        &mut self,
        player_id: PlayerId,
        unregister_observer: ClientAddr<G>,
        players: &PlayerRepo<G>,
    ) {
        // There is a possible race condition to handle:
        //  1. Client A registers
        //  3. Client B registers with the same session and player so evicts client A from limbo
        //  2. Client A unregisters and is placed in limbo

        let mut player = match players.borrow_player_mut(player_id) {
            Some(player) => player,
            None => return,
        };

        let client = match player.client_mut() {
            Some(client) => client,
            None => return,
        };

        match &client.status {
            ClientStatus::Connected { observer } => {
                if observer.same_channel(&unregister_observer) {
                    client.status = ClientStatus::Limbo {
                        expiry: Instant::now() + G::LIMBO,
                    };
                    info!("player {:?} is in limbo", player_id);
                }
            }
            _ => {}
        }
    }

    /// Update all clients with game state.
    pub fn update(
        &mut self,
        game: &G,
        players: &mut PlayerRepo<G>,
        teams: &mut TeamRepo<G>,
        liveboard: &mut LiveboardRepo<G>,
        leaderboard: &LeaderboardRepo<G>,
        server_delta: Option<(Arc<[ServerDto]>, Arc<[ServerId]>)>,
        counter: Ticks,
    ) {
        benchmark_scope!("update_clients");

        let player_update = players.delta(&*teams);
        let team_update = teams.delta();
        let immut_players = &*players;
        let player_chat_team_updates: HashMap<PlayerId, _> = players
            .iter_player_ids()
            .filter(|&id| {
                !id.is_bot()
                    && immut_players
                        .borrow_player(id)
                        .unwrap()
                        .client()
                        .map(|c| matches!(c.status, ClientStatus::Connected { .. }))
                        .unwrap_or(false)
            })
            .map(|player_id| {
                (
                    player_id,
                    (
                        ChatRepo::<G>::player_delta(player_id, immut_players),
                        teams.player_delta(player_id, immut_players).unwrap(),
                    ),
                )
            })
            .collect();
        let liveboard_update = liveboard.delta(&*players, &*teams);
        let leaderboard_update: Vec<_> = leaderboard.deltas_nondestructive().collect();

        players.players.par_iter().for_each(
            move |(player_id, player_tuple): (&PlayerId, &Arc<PlayerTuple<G>>)| {
                let player = player_tuple.borrow_player();

                let client_data = match player.client() {
                    Some(client) => client,
                    None => return,
                };

                // In limbo or will be soon (not connected, cannot send an update).
                if let ClientStatus::Connected { observer } = &client_data.status {
                    if let Some(update) = game.get_client_update(
                        counter,
                        player_tuple,
                        &mut *client_data.data.borrow_mut(),
                    ) {
                        let _ = observer.send(ObserverUpdate::Send {
                            message: Update::Game(update),
                        });
                    }

                    if let Some((added, removed, real_players)) = player_update.as_ref() {
                        let _ = observer.send(ObserverUpdate::Send {
                            message: Update::Player(PlayerUpdate::Updated {
                                added: Arc::clone(added),
                                removed: Arc::clone(removed),
                                real_players: *real_players,
                            }),
                        });
                    }

                    if let Some((added, removed)) = team_update.as_ref() {
                        if !added.is_empty() {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Team(TeamUpdate::AddedOrUpdated(Arc::clone(
                                    added,
                                ))),
                            });
                        }
                        if !removed.is_empty() {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Team(TeamUpdate::Removed(Arc::clone(removed))),
                            });
                        }
                    }

                    if let Some((chat_update, (members, joiners, joins))) =
                        player_chat_team_updates.get(&player_id)
                    {
                        if let Some(chat_update) = chat_update {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Chat(chat_update.clone()),
                            });
                        }

                        // TODO: We could get members on a per team basis.
                        if let Some(members) = members {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Team(TeamUpdate::Members(
                                    members.deref().clone().into(),
                                )),
                            });
                        }

                        if let Some(joiners) = joiners {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Team(TeamUpdate::Joiners(
                                    joiners.deref().clone().into(),
                                )),
                            });
                        }

                        if let Some(joins) = joins {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::Team(TeamUpdate::Joins(
                                    joins.iter().cloned().collect(),
                                )),
                            });
                        }
                    } else {
                        debug_assert!(
                            false,
                            "not possible, all connected clients should have an entry"
                        );
                    }

                    for &(period_id, leaderboard) in &leaderboard_update {
                        let _ = observer.send(ObserverUpdate::Send {
                            message: Update::Leaderboard(LeaderboardUpdate::Updated(
                                period_id,
                                Arc::clone(&leaderboard),
                            )),
                        });
                    }

                    if let Some((added, removed)) = liveboard_update.as_ref() {
                        let _ = observer.send(ObserverUpdate::Send {
                            message: Update::Liveboard(LiveboardUpdate::Updated {
                                added: Arc::clone(added),
                                removed: Arc::clone(removed),
                            }),
                        });
                    }

                    if let Some((added, removed)) = server_delta.as_ref() {
                        if !added.is_empty() {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::System(SystemUpdate::Added(Arc::clone(added))),
                            });
                        }
                        if !removed.is_empty() {
                            let _ = observer.send(ObserverUpdate::Send {
                                message: Update::System(SystemUpdate::Removed(Arc::clone(removed))),
                            });
                        }
                    }
                }
            },
        );
    }

    /// Cleans up old clients.
    pub(crate) fn prune(
        &mut self,
        service: &mut G,
        players: &mut PlayerRepo<G>,
        teams: &mut TeamRepo<G>,
        invitations: &mut InvitationRepo<G>,
        metrics: &mut MetricRepo<G>,
    ) {
        benchmark_scope!("prune_clients");

        let now = Instant::now();
        let to_forget: Vec<PlayerId> = players
            .players
            .iter()
            .filter(|&(player_id, player_tuple)| {
                let mut player = player_tuple.borrow_player_mut();
                if let Some(client_data) = player.client_mut() {
                    match &client_data.status {
                        ClientStatus::Connected { .. } => {
                            // Wait for transition to limbo via unregister, which is the "proper" channel.
                            false
                        }
                        ClientStatus::Limbo { expiry } => {
                            if &now >= expiry {
                                client_data.status = ClientStatus::Stale {
                                    expiry: Instant::now() + ClientStatus::<G>::STALE_EXPIRY,
                                };
                                drop(player);
                                service.player_left(player_tuple);
                                info!("player_id {:?} expired from limbo", player_id);
                            }
                            false
                        }
                        // Not actually in game, so no cleanup required.
                        ClientStatus::Pending { expiry } | ClientStatus::Stale { expiry } => {
                            &now > expiry
                        }
                    }
                } else {
                    false
                }
            })
            .map(|(&player_id, _)| player_id)
            .collect();

        for player_id in to_forget {
            players.forget(player_id, teams, invitations, metrics);
        }
    }

    /// Filter-map-reduce all [`ClientData<G>`]'s.
    ///
    /// That is to say, apply a function that optionally produces some type T. Return a sorted
    /// mapping of T to the fraction of corresponding clients.
    pub(crate) fn filter_map_reduce<T: Hash + Eq>(
        players: &PlayerRepo<G>,
        fmr: impl Fn(&PlayerClientData<G>) -> Option<T>,
    ) -> Vec<(T, f32)> {
        let mut hash: HashMap<T, u32> = HashMap::new();
        let mut total = 0u32;
        for player_data in players.iter_borrow() {
            if let Some(client_data) = player_data.client() {
                total += 1;
                if let Some(v) = fmr(client_data) {
                    *hash.entry(v).or_insert(0) += 1;
                }
            }
        }
        let mut list: Vec<(T, u32)> = hash.into_iter().collect();
        // Sort in reverse so higher counts are first.
        list.sort_unstable_by_key(|(_, count)| u32::MAX - count);
        list.into_iter()
            .map(|(v, count)| (v, count as f32 / total as f32))
            .collect()
    }

    /// Handles [`G::Command`]'s.
    fn handle_game_command(
        player_id: PlayerId,
        command: G::Command,
        service: &mut G,
        players: &PlayerRepo<G>,
    ) -> Result<Option<G::ClientUpdate>, &'static str> {
        if let Some(player_data) = players.get(player_id) {
            // Game updates for all players are usually processed at once, but we also allow
            // one-off responses.
            Ok(service.player_command(command, player_data))
        } else {
            Err("nonexistent observer")
        }
    }

    /// Request a different alias (may not be done while alive).
    fn set_alias(
        player_id: PlayerId,
        alias: PlayerAlias,
        players: &PlayerRepo<G>,
    ) -> Result<ClientUpdate, &'static str> {
        let mut player = players
            .borrow_player_mut(player_id)
            .ok_or("player doesn't exist")?;

        if player
            .alive_duration()
            .map(|d| d > Duration::from_secs(1))
            .unwrap_or(false)
        {
            return Err("cannot change alias while alive");
        }

        let client = player.client_mut().ok_or("only clients can set alias")?;
        let censored_alias = PlayerAlias::new_sanitized(alias.as_str());
        client.alias = censored_alias;
        Ok(ClientUpdate::AliasSet(censored_alias))
    }

    /// Record client frames per second (FPS) for statistical purposes.
    fn tally_fps(
        player_id: PlayerId,
        fps: f32,
        players: &PlayerRepo<G>,
    ) -> Result<ClientUpdate, &'static str> {
        let mut player = players
            .borrow_player_mut(player_id)
            .ok_or("player doesn't exist")?;
        let client = player.client_mut().ok_or("only clients can tally fps")?;

        client.metrics.fps = sanitize_tps(fps);
        if client.metrics.fps.is_some() {
            Ok(ClientUpdate::FpsTallied)
        } else {
            Err("invalid fps")
        }
    }

    /// Record a client-side error message for investigation.
    fn trace(
        player_id: PlayerId,
        message: String,
        players: &PlayerRepo<G>,
        trace_log: Option<&str>,
    ) -> Result<ClientUpdate, &'static str> {
        let mut player = players
            .borrow_player_mut(player_id)
            .ok_or("player doesn't exist")?;
        let client = player.client_mut().ok_or("only clients can trace")?;

        if message.len() > 2048 {
            Err("trace too long")
        } else if client.traces < 25 {
            if let Some(trace_log) = trace_log {
                match OpenOptions::new().create(true).append(true).open(trace_log) {
                    Ok(mut file) => {
                        if let Err(e) = write!(
                            file,
                            "{}",
                            format!(
                                "ref={:?}, reg={:?}, ua={:?}, msg={}\n",
                                client.metrics.referrer,
                                client.metrics.region_id,
                                client.metrics.user_agent_id,
                                message
                            )
                        ) {
                            error!("error logging trace to file: {:?}", e);
                        }
                    }
                    Err(e) => error!("could not open file for traces: {:?}", e),
                }
            } else {
                info!("client_trace: {}", message);
            }
            client.traces += 1;
            Ok(ClientUpdate::Traced)
        } else {
            Err("too many traces")
        }
    }

    /// Handles an arbitrary [`ClientRequest`].
    fn handle_client_request(
        &mut self,
        player_id: PlayerId,
        request: ClientRequest,
        players: &PlayerRepo<G>,
    ) -> Result<ClientUpdate, &'static str> {
        match request {
            ClientRequest::SetAlias(alias) => Self::set_alias(player_id, alias, players),
            ClientRequest::TallyFps(fps) => Self::tally_fps(player_id, fps, players),
            ClientRequest::Trace { message } => {
                Self::trace(player_id, message, players, self.trace_log.as_deref())
            }
        }
    }

    /// Handles request made by real player.
    fn handle_observer_request(
        &mut self,
        player_id: PlayerId,
        request: Request<G::Command>,
        service: &mut G,
        arena_id: ArenaId,
        server_id: Option<ServerId>,
        players: &mut PlayerRepo<G>,
        teams: &mut TeamRepo<G>,
        chat: &mut ChatRepo<G>,
        invitations: &mut InvitationRepo<G>,
        metrics: &mut MetricRepo<G>,
    ) -> Result<Option<Update<G::ClientUpdate>>, &'static str> {
        match request {
            // Goes first (fast path).
            Request::Game(command) => {
                Self::handle_game_command(player_id, command, service, &*players)
                    .map(|u| u.map(Update::Game))
            }
            Request::Client(request) => self
                .handle_client_request(player_id, request, &*players)
                .map(|u| Some(Update::Client(u))),
            Request::Chat(request) => chat
                .handle_chat_request(player_id, request, players, teams, metrics)
                .map(|u| Some(Update::Chat(u))),
            Request::Invitation(request) => invitations
                .handle_invitation_request(player_id, request, arena_id, server_id, players)
                .map(|u| Some(Update::Invitation(u))),
            Request::Player(request) => players
                .handle_player_request(player_id, request, metrics)
                .map(|u| Some(Update::Player(u))),
            Request::Team(request) => teams
                .handle_team_request(player_id, request, players)
                .map(|u| Some(Update::Team(u))),
        }
    }

    /// Record network round-trip-time measured by websocket for statistical purposes.
    fn handle_observer_rtt(&mut self, player_id: PlayerId, rtt: u16, players: &PlayerRepo<G>) {
        let mut player = match players.borrow_player_mut(player_id) {
            Some(player) => player,
            None => return,
        };

        let client = match player.client_mut() {
            Some(client) => client,
            None => return,
        };

        client.metrics.rtt = Some(rtt);
    }
}

/// Don't let bad values sneak in.
fn sanitize_tps(tps: f32) -> Option<f32> {
    tps.is_finite().then_some(tps.clamp(0.0, 144.0))
}

/// Data stored per client (a.k.a websocket a.k.a. real player).
#[derive(Debug)]
pub(crate) struct PlayerClientData<G: GameArenaService> {
    /// Authentication.
    pub session_id: SessionId,
    /// Alias chosen by player.
    pub alias: PlayerAlias,
    /// Connection state.
    pub status: ClientStatus<G>,
    /// Previous database item.
    pub session_item: Option<SessionItem>,
    pub metrics: ClientMetricData<G>,
    pub(crate) invitation: ClientInvitationData,
    pub(crate) chat: ClientChatData,
    pub(crate) team: ClientTeamData,
    /// Players this client has reported.
    pub(crate) reported: HashSet<PlayerId>,
    /// Number of times sent error trace (in order to limit abuse).
    pub traces: u8,
    /// Game specific client data. Manually serialized
    data: AtomicRefCell<G::ClientData>,
}

#[derive(Debug)]
pub(crate) enum ClientStatus<G: GameArenaService> {
    /// Pending: Initial state. Can be forgotten after expiry.
    Pending { expiry: Instant },
    /// Connected and in game. Transitions to limbo if the connection is lost.
    Connected { observer: ClientAddr<G> },
    /// Disconnected but still in game.
    /// - Transitions to connected if a new connection is established.
    /// - Transitions to stale after expiry.
    Limbo { expiry: Instant },
    /// Disconnected and out of game.
    /// - Transitions to connected if a new connection is established.
    /// - Client can be forgotten after expiry.
    Stale { expiry: Instant },
}

impl<G: GameArenaService> ClientStatus<G> {
    /// How long into the future to set stale expiry when a client expires from limbo. In debug mode,
    /// this is shorter to facilitate testing.
    #[cfg(debug_assertions)]
    pub const STALE_EXPIRY: Duration = Duration::from_secs(75);
    /// How long into the future to set stale expiry when a client expires from limbo. In release mode,
    /// this is longer to reduce expensive database lookups.
    #[cfg(not(debug_assertions))]
    pub const STALE_EXPIRY: Duration = Duration::from_secs(48 * 3600);
}

impl<G: GameArenaService> PlayerClientData<G> {
    pub fn new(
        session_id: SessionId,
        metrics: ClientMetricData<G>,
        invitation: Option<InvitationDto>,
    ) -> Self {
        Self {
            session_id,
            alias: G::default_alias(),
            status: ClientStatus::Pending {
                expiry: Instant::now() + Duration::from_secs(10),
            },
            session_item: None,
            metrics,
            invitation: ClientInvitationData::new(invitation),
            chat: ClientChatData::default(),
            team: ClientTeamData::default(),
            reported: Default::default(),
            traces: 0,
            data: AtomicRefCell::new(G::ClientData::default()),
        }
    }
}

/// Handle client messages.
impl<G: GameArenaService> Handler<ObserverMessage<Request<G::Command>, Update<G::ClientUpdate>>>
    for Infrastructure<G>
{
    type Result = ();

    fn handle(
        &mut self,
        msg: ObserverMessage<Request<G::Command>, Update<G::ClientUpdate>>,
        _ctx: &mut Self::Context,
    ) {
        match msg {
            ObserverMessage::Register {
                player_id,
                observer,
                ..
            } => self.context_service.context.clients.register(
                player_id,
                observer,
                &mut self.context_service.context.players,
                &mut self.context_service.context.teams,
                &self.context_service.context.chat,
                &self.leaderboard,
                &self.context_service.context.liveboard,
                self.system.as_ref(),
                self.context_service.context.arena_id,
                self.server_id,
                &mut self.context_service.service,
            ),
            ObserverMessage::Unregister {
                player_id,
                observer,
            } => self.context_service.context.clients.unregister(
                player_id,
                observer,
                &self.context_service.context.players,
            ),
            ObserverMessage::Request { player_id, request } => {
                let context = &mut self.context_service.context;
                let service = &mut self.context_service.service;
                match context.clients.handle_observer_request(
                    player_id,
                    request,
                    service,
                    context.arena_id,
                    self.server_id,
                    &mut context.players,
                    &mut context.teams,
                    &mut context.chat,
                    &mut self.invitations,
                    &mut self.metrics,
                ) {
                    Ok(Some(message)) => {
                        let player = match context.players.borrow_player_mut(player_id) {
                            Some(player) => player,
                            None => {
                                debug_assert!(false);
                                return;
                            }
                        };

                        let client = match player.client() {
                            Some(client) => client,
                            None => {
                                debug_assert!(false);
                                return;
                            }
                        };

                        if let ClientStatus::Connected { observer } = &client.status {
                            let _ = observer.send(ObserverUpdate::Send { message });
                        } else {
                            debug_assert!(false, "impossible due to synchronous nature of code");
                        }
                    }
                    Ok(None) => {}
                    Err(s) => {
                        warn!("observer request resulted in {}", s);
                    }
                }
            }
            ObserverMessage::RoundTripTime { player_id, rtt } => self
                .context_service
                .context
                .clients
                .handle_observer_rtt(player_id, rtt, &self.context_service.context.players),
        }
    }
}

#[derive(Message)]
#[rtype(result = "Result<PlayerId, &'static str>")]
pub struct Authenticate {
    /// Client ip address.
    pub ip_address: Option<IpAddr>,
    /// User agent.
    pub user_agent_id: Option<UserAgentId>,
    /// Referrer.
    pub referrer: Option<Referrer>,
    /// Last valid credentials.
    pub arena_id_session_id: Option<(ArenaId, SessionId)>,
    /// Invitation?
    pub invitation_id: Option<InvitationId>,
}

impl<G: GameArenaService> Handler<Authenticate> for Infrastructure<G> {
    type Result = ResponseActFuture<Self, Result<PlayerId, &'static str>>;

    fn handle(&mut self, msg: Authenticate, _ctx: &mut ActorContext<Self>) -> Self::Result {
        let arena_id = self.context_service.context.arena_id;
        let clients = &mut self.context_service.context.clients;
        let players = &self.context_service.context.players;

        if msg
            .ip_address
            .map(|ip| clients.authenticate_rate_limiter.should_limit_rate(ip))
            .unwrap_or(false)
        {
            // Should only log IP of malicious actors.
            warn!("IP {:?} was rate limited", msg.ip_address);
            return Box::pin(fut::ready(Err("rate limit exceeded")));
        }

        // TODO: O(n) on players.
        let cached_session_id_player_id = msg
            .arena_id_session_id
            .filter(|&(msg_arena_id, _)| arena_id == msg_arena_id)
            .and_then(|(_, msg_session_id)| {
                players
                    .iter_borrow()
                    .find(|p| {
                        p.client()
                            .map(|c| c.session_id == msg_session_id)
                            .unwrap_or(false)
                    })
                    .map(|p| (msg_session_id, p.player_id))
            });

        let arena_id_session_id = msg.arena_id_session_id;
        let database = self.database();

        Box::pin(
            async move {
                if cached_session_id_player_id.is_some() {
                    // No need to load from database because session is in memory.
                    Result::Ok(None)
                } else if let Some((arena_id, session_id)) = arena_id_session_id {
                    database.get_session(arena_id, session_id).await
                } else {
                    // Cannot load from database because (arena_id, session_id) is unavailable.
                    Result::Ok(None)
                }
            }
            .into_actor(self)
            .map(move |db_result, act, _ctx| {
                let invitation = msg
                    .invitation_id
                    .and_then(|id| act.invitations.get(id).cloned());
                let invitation_dto = invitation.map(|i| InvitationDto {
                    player_id: i.player_id,
                });

                let mut client_metric_data = ClientMetricData::from(&msg);

                act.metrics.start_session(
                    &msg,
                    invitation_dto.is_some(),
                    db_result.as_ref().map(|r| r.is_some()).unwrap_or(false),
                    &client_metric_data,
                );

                let (session_id, player_id) =
                    if let Some(cached_session_id_player_id) = cached_session_id_player_id {
                        cached_session_id_player_id
                    } else if let Ok(Some(session_item)) = db_result {
                        client_metric_data.supplement(&session_item);
                        (session_item.session_id, session_item.player_id)
                    } else {
                        // TODO: O(n) on players.
                        let mut session_ids = HashSet::with_capacity(
                            act.context_service.context.players.real_players_live,
                        );

                        for player in act.context_service.context.players.iter_borrow() {
                            if let Some(client_data) = player.client() {
                                session_ids.insert(client_data.session_id);
                            }
                        }

                        let new_session_id = loop {
                            let session_id = SessionId(generate_id_64());
                            if !session_ids.contains(&session_id) {
                                break session_id;
                            }
                        };

                        let new_player_id = loop {
                            let player_id = PlayerId(generate_id());
                            if !act.context_service.context.players.contains(player_id) {
                                break player_id;
                            }
                        };

                        (new_session_id, new_player_id)
                    };

                match act.context_service.context.players.players.entry(player_id) {
                    Entry::Occupied(mut occupied) => {
                        if let Some(client) = occupied.get_mut().borrow_player_mut().client_mut() {
                            client.metrics.date_renewed = get_unix_time_now();
                        } else {
                            debug_assert!(false, "impossible to be a bot since session was valid");
                        }
                    }
                    Entry::Vacant(vacant) => {
                        let client =
                            PlayerClientData::new(session_id, client_metric_data, invitation_dto);
                        let pd = PlayerData::new(player_id, Some(Box::new(client)));
                        let pt = Arc::new(PlayerTuple::new(pd));
                        vacant.insert(pt);
                    }
                }

                Ok(player_id)
            }),
        )
    }
}