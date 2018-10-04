//! Management of connected clients to a WebPush server
//!
//! This module is a pretty heavy work in progress. The intention is that
//! this'll house all the various state machine transitions and state management
//! of connected clients. Note that it's expected there'll be a lot of connected
//! clients, so this may appears relatively heavily optimized!
use std::cell::RefCell;
use std::mem;
use std::rc::Rc;
use std::time::Duration;

use cadence::{prelude::*, StatsdClient};
use error_chain::ChainedError;
use futures::future;
use futures::future::Either;
use futures::sync::mpsc;
use futures::sync::oneshot::Receiver;
use futures::AsyncSink;
use futures::{Async, Future, Poll, Sink, Stream};
use reqwest::async::Client as AsyncClient;
use rusoto_dynamodb::UpdateItemOutput;
use sentry;
use sentry::integrations::error_chain::event_from_error_chain;
use state_machine_future::RentToOwn;
use tokio_core::reactor::Timeout;
use uuid::Uuid;
use woothee::parser::Parser;

use db::{CheckStorageResponse, HelloResponse, RegisterResponse};
use errors::*;
use protocol::{ClientMessage, Notification, ServerMessage, ServerNotification};
use server::Server;
use util::megaphone::{Broadcast, BroadcastSubs};
use util::{ms_since_epoch, parse_user_agent, sec_since_epoch};

// Created and handed to the AutopushServer
pub struct RegisteredClient {
    pub uaid: Uuid,
    pub uid: Uuid,
    pub tx: mpsc::UnboundedSender<ServerNotification>,
}

pub struct Client<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    state_machine: UnAuthClientStateFuture<T>,
    srv: Rc<Server>,
    broadcast_subs: Rc<RefCell<BroadcastSubs>>,
    tx: mpsc::UnboundedSender<ServerNotification>,
}

impl<T> Client<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    /// Spins up a new client communicating over the websocket `ws` specified.
    ///
    /// The `ws` specified already has ping/pong parts of the websocket
    /// protocol managed elsewhere, and this struct is only expected to deal
    /// with webpush-specific messages.
    ///
    /// The `srv` argument is the server that this client is attached to and
    /// the various state behind the server. This provides transitive access to
    /// various configuration options of the server as well as the ability to
    /// call back into Python.
    pub fn new(ws: T, srv: &Rc<Server>, mut uarx: Receiver<String>) -> Client<T> {
        let srv = srv.clone();
        let timeout = Timeout::new(srv.opts.open_handshake_timeout.unwrap(), &srv.handle).unwrap();
        let (tx, rx) = mpsc::unbounded();

        // Pull out the user-agent, which we should have by now
        let uastr = match uarx.poll() {
            Ok(Async::Ready(ua)) => ua,
            Ok(Async::NotReady) => {
                error!("Failed to parse the user-agent");
                String::from("")
            }
            Err(_) => {
                error!("Failed to receive a value");
                String::from("")
            }
        };

        let broadcast_subs = Rc::new(RefCell::new(Default::default()));
        let sm = UnAuthClientState::start(
            UnAuthClientData {
                srv: srv.clone(),
                ws,
                user_agent: uastr,
                broadcast_subs: broadcast_subs.clone(),
            },
            timeout,
            tx.clone(),
            rx,
        );

        Self {
            state_machine: sm,
            srv: srv.clone(),
            broadcast_subs,
            tx,
        }
    }

    pub fn broadcast_delta(&mut self) -> Option<Vec<Broadcast>> {
        let mut broadcast_subs = self.broadcast_subs.borrow_mut();
        self.srv.broadcast_delta(&mut broadcast_subs)
    }

    pub fn shutdown(&mut self) {
        let _result = self.tx.unbounded_send(ServerNotification::Disconnect);
    }
}

impl<T> Future for Client<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    type Item = ();
    type Error = Error;

    fn poll(&mut self) -> Poll<(), Error> {
        self.state_machine.poll()
    }
}

// Websocket session statistics
#[derive(Clone, Default)]
struct SessionStatistics {
    // User data
    uaid: String,
    uaid_reset: bool,
    existing_uaid: bool,
    connection_type: String,

    // Usage data
    direct_acked: i32,
    direct_storage: i32,
    stored_retrieved: i32,
    stored_acked: i32,
    nacks: i32,
    unregisters: i32,
    registers: i32,
}

// Represent the state for a valid WebPush client that is authenticated
pub struct WebPushClient {
    uaid: Uuid,
    uid: Uuid,
    rx: mpsc::UnboundedReceiver<ServerNotification>,
    flags: ClientFlags,
    message_month: String,
    unacked_direct_notifs: Vec<Notification>,
    unacked_stored_notifs: Vec<Notification>,
    // Highest version from stored, retained for use with increment
    // when all the unacked storeds are ack'd
    unacked_stored_highest: Option<u64>,
    connected_at: u64,
    sent_from_storage: u32,
    last_ping: u64,
    stats: SessionStatistics,
}

impl Default for WebPushClient {
    fn default() -> Self {
        let (_, rx) = mpsc::unbounded();
        Self {
            uaid: Default::default(),
            uid: Default::default(),
            rx,
            flags: Default::default(),
            message_month: Default::default(),
            unacked_direct_notifs: Default::default(),
            unacked_stored_notifs: Default::default(),
            unacked_stored_highest: Default::default(),
            connected_at: Default::default(),
            sent_from_storage: Default::default(),
            last_ping: Default::default(),
            stats: Default::default(),
        }
    }
}

impl WebPushClient {
    fn unacked_messages(&self) -> bool {
        !self.unacked_stored_notifs.is_empty() || !self.unacked_direct_notifs.is_empty()
    }
}

#[derive(Default)]
pub struct ClientFlags {
    /// Whether check_storage queries for topic (not "timestamped") messages
    include_topic: bool,
    /// Flags the need to increment the last read for timestamp for timestamped messages
    increment_storage: bool,
    /// Whether this client needs to check storage for messages
    check: bool,
    /// Flags the need to drop the user record
    reset_uaid: bool,
    rotate_message_table: bool,
}

impl ClientFlags {
    fn new() -> Self {
        Self {
            include_topic: true,
            increment_storage: false,
            check: false,
            reset_uaid: false,
            rotate_message_table: false,
        }
    }
}

pub struct UnAuthClientData<T> {
    srv: Rc<Server>,
    ws: T,
    user_agent: String,
    broadcast_subs: Rc<RefCell<BroadcastSubs>>,
}

impl<T> UnAuthClientData<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    fn input_with_timeout(&mut self, timeout: &mut Timeout) -> Poll<ClientMessage, Error> {
        let item = match timeout.poll()? {
            Async::Ready(_) => return Err("Client timed out".into()),
            Async::NotReady => match self.ws.poll()? {
                Async::Ready(None) => return Err("Client dropped".into()),
                Async::Ready(Some(msg)) => Async::Ready(msg),
                Async::NotReady => Async::NotReady,
            },
        };
        Ok(item)
    }
}

pub struct AuthClientData<T> {
    srv: Rc<Server>,
    ws: T,
    webpush: Rc<RefCell<WebPushClient>>,
    broadcast_subs: Rc<RefCell<BroadcastSubs>>,
}

impl<T> AuthClientData<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    fn input_or_notif(&mut self) -> Poll<Either<ClientMessage, ServerNotification>, Error> {
        let mut webpush = self.webpush.borrow_mut();
        let item = match webpush.rx.poll() {
            Ok(Async::Ready(Some(notif))) => Either::B(notif),
            Ok(Async::Ready(None)) => return Err("Sending side dropped".into()),
            Ok(Async::NotReady) => match self.ws.poll()? {
                Async::Ready(None) => return Err("Client dropped".into()),
                Async::Ready(Some(msg)) => Either::A(msg),
                Async::NotReady => return Ok(Async::NotReady),
            },
            Err(_) => return Err("Unexpected error".into()),
        };
        Ok(Async::Ready(item))
    }
}

#[derive(StateMachineFuture)]
pub enum UnAuthClientState<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    #[state_machine_future(start, transitions(AwaitProcessHello))]
    AwaitHello {
        data: UnAuthClientData<T>,
        timeout: Timeout,
        tx: mpsc::UnboundedSender<ServerNotification>,
        rx: mpsc::UnboundedReceiver<ServerNotification>,
    },

    #[state_machine_future(transitions(AwaitSessionComplete))]
    AwaitProcessHello {
        response: MyFuture<HelloResponse>,
        data: UnAuthClientData<T>,
        desired_broadcasts: Vec<Broadcast>,
        tx: mpsc::UnboundedSender<ServerNotification>,
        rx: mpsc::UnboundedReceiver<ServerNotification>,
    },

    #[state_machine_future(transitions(UnAuthDone))]
    AwaitSessionComplete {
        auth_state_machine: AuthClientStateFuture<T>,
        srv: Rc<Server>,
        user_agent: String,
        webpush: Rc<RefCell<WebPushClient>>,
    },

    #[state_machine_future(ready)]
    UnAuthDone(()),

    #[state_machine_future(error)]
    UnAuthClientStateError(Error),
}

impl<T> PollUnAuthClientState<T> for UnAuthClientState<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    fn poll_await_hello<'a>(
        hello: &'a mut RentToOwn<'a, AwaitHello<T>>,
    ) -> Poll<AfterAwaitHello<T>, Error> {
        trace!("State: AwaitHello");
        let (uaid, desired_broadcasts) = {
            let AwaitHello {
                ref mut data,
                ref mut timeout,
                ..
            } = **hello;
            match try_ready!(data.input_with_timeout(timeout)) {
                ClientMessage::Hello {
                    uaid,
                    use_webpush: Some(true),
                    broadcasts,
                    ..
                } => (
                    uaid.and_then(|uaid| Uuid::parse_str(uaid.as_str()).ok()),
                    Broadcast::from_hashmap(broadcasts.unwrap_or_default()),
                ),
                _ => return Err("Invalid message, must be hello".into()),
            }
        };

        let AwaitHello { data, tx, rx, .. } = hello.take();
        let connected_at = ms_since_epoch();
        let response = Box::new(data.srv.ddb.hello(
            &connected_at,
            uaid.as_ref(),
            &data.srv.opts.router_url,
        ));
        transition!(AwaitProcessHello {
            response,
            data,
            desired_broadcasts,
            tx,
            rx,
        })
    }

    fn poll_await_process_hello<'a>(
        process_hello: &'a mut RentToOwn<'a, AwaitProcessHello<T>>,
    ) -> Poll<AfterAwaitProcessHello<T>, Error> {
        trace!("State: AwaitProcessHello");
        let (uaid, message_month, check_storage, reset_uaid, rotate_message_table, connected_at) = {
            match try_ready!(process_hello.response.poll()) {
                HelloResponse {
                    uaid: Some(uaid),
                    message_month,
                    check_storage,
                    reset_uaid,
                    rotate_message_table,
                    connected_at,
                } => (
                    uaid,
                    message_month,
                    check_storage,
                    reset_uaid,
                    rotate_message_table,
                    connected_at,
                ),
                HelloResponse { uaid: None, .. } => {
                    return Err("Already connected elsewhere".into())
                }
            }
        };

        let AwaitProcessHello {
            data,
            desired_broadcasts,
            tx,
            rx,
            ..
        } = process_hello.take();
        data.srv.metrics.incr("ua.command.hello").ok();

        let UnAuthClientData {
            srv,
            ws,
            user_agent,
            broadcast_subs,
        } = data;

        // Setup the objects and such needed for a WebPushClient
        let mut flags = ClientFlags::new();
        flags.check = check_storage;
        flags.reset_uaid = reset_uaid;
        flags.rotate_message_table = rotate_message_table;
        let (initialized_subs, broadcasts) = srv.broadcast_init(&desired_broadcasts);
        broadcast_subs.replace(initialized_subs);
        let uid = Uuid::new_v4();
        let webpush = Rc::new(RefCell::new(WebPushClient {
            uaid,
            uid: uid,
            flags,
            rx,
            message_month,
            connected_at,
            stats: SessionStatistics {
                uaid: uaid.to_simple().to_string(),
                uaid_reset: reset_uaid,
                existing_uaid: check_storage,
                connection_type: String::from("webpush"),
                ..Default::default()
            },
            ..Default::default()
        }));
        srv.clients.connect(RegisteredClient { uaid, uid, tx })?;

        let response = ServerMessage::Hello {
            uaid: uaid.to_simple().to_string(),
            status: 200,
            use_webpush: Some(true),
            broadcasts,
        };
        let auth_state_machine = AuthClientState::start(
            vec![response],
            AuthClientData {
                srv: srv.clone(),
                ws,
                webpush: webpush.clone(),
                broadcast_subs: broadcast_subs.clone(),
            },
        );
        transition!(AwaitSessionComplete {
            auth_state_machine,
            srv,
            user_agent,
            webpush,
        })
    }

    fn poll_await_session_complete<'a>(
        session_complete: &'a mut RentToOwn<'a, AwaitSessionComplete<T>>,
    ) -> Poll<AfterAwaitSessionComplete, Error> {
        let error = {
            match session_complete.auth_state_machine.poll() {
                Ok(Async::NotReady) => return Ok(Async::NotReady),
                Ok(Async::Ready(_))
                | Err(Error(ErrorKind::Ws(_), _))
                | Err(Error(ErrorKind::Io(_), _))
                | Err(Error(ErrorKind::PongTimeout, _))
                | Err(Error(ErrorKind::RepeatUaidDisconnect, _))
                | Err(Error(ErrorKind::ExcessivePing, _))
                | Err(Error(ErrorKind::InvalidStateTransition(_, _), _)) => None,
                Err(e) => Some(e),
            }
        };

        let AwaitSessionComplete {
            srv,
            user_agent,
            webpush,
            ..
        } = session_complete.take();
        let mut webpush = webpush.borrow_mut();
        // If there's any notifications in the queue, move them to our unacked direct notifs
        webpush.rx.close();
        loop {
            match webpush.rx.poll() {
                Ok(Async::Ready(Some(msg))) => match msg {
                    ServerNotification::CheckStorage | ServerNotification::Disconnect => continue,
                    ServerNotification::Notification(notif) => {
                        webpush.unacked_direct_notifs.push(notif)
                    }
                },
                Ok(Async::Ready(None)) | Ok(Async::NotReady) | Err(_) => break,
            }
        }
        let now = ms_since_epoch();
        let elapsed = (now - webpush.connected_at) / 1_000;
        let parser = Parser::new();
        let (ua_result, metrics_os, metrics_browser) = parse_user_agent(&parser, &user_agent);
        // dogstatsd doesn't support timers: use histogram instead
        srv.metrics
            .time_with_tags("ua.connection.lifespan", elapsed)
            .with_tag("ua_os_family", metrics_os)
            .with_tag("ua_browser_family", metrics_browser)
            .send();

        let _ = srv.clients.disconnect(&webpush.uaid, &webpush.uid);

        // Log out the sentry message if applicable and convert to error msg
        let error = if let Some(ref err) = error {
            let mut event = event_from_error_chain(err);
            event.user = Some(sentry::User {
                id: Some(webpush.uaid.to_simple().to_string()),
                ..Default::default()
            });
            event.tags.insert("ua_name".to_string(), ua_result.name.to_string());
            event.tags.insert("ua_os_family".to_string(), metrics_os.to_string());
            event.tags.insert("ua_os_ver".to_string(), ua_result.os_version.to_string());
            event.tags.insert("ua_browser_family".to_string(), metrics_browser.to_string());
            event.tags.insert("ua_browser_ver".to_string(), ua_result.version.to_string());
            sentry::capture_event(event);
            err.display_chain().to_string()
        } else {
            "".to_string()
        };

        // If there's direct unack'd messages, they need to be saved out without blocking
        // here
        let mut stats = webpush.stats.clone();
        let unacked_direct_notifs = webpush.unacked_direct_notifs.len();
        if unacked_direct_notifs > 0 {
            debug!("Writing direct notifications to storage");
            stats.direct_storage += unacked_direct_notifs as i32;
            let mut notifs = mem::replace(&mut webpush.unacked_direct_notifs, Vec::new());
            // Ensure we don't store these as legacy by setting a 0 as the sortkey_timestamp
            // That will ensure the Python side doesn't mark it as legacy during conversion and
            // still get the correct default us_time when saving.
            for notif in &mut notifs {
                notif.sortkey_timestamp = Some(0);
            }
            let srv1 = srv.clone();
            save_and_notify_undelivered_messages(&webpush, srv1, notifs);
        }

        // Log out the final stats message
        info!("Session";
        "uaid_hash" => &stats.uaid,
        "uaid_reset" => stats.uaid_reset,
        "existing_uaid" => stats.existing_uaid,
        "connection_type" => &stats.connection_type,
        "ua_name" => ua_result.name,
        "ua_os_family" => metrics_os,
        "ua_os_ver" => ua_result.os_version,
        "ua_browser_family" => metrics_browser,
        "ua_browser_ver" => ua_result.version,
        "ua_category" => ua_result.category,
        "connection_time" => elapsed,
        "direct_acked" => stats.direct_acked,
        "direct_storage" => stats.direct_storage,
        "stored_retrieved" => stats.stored_retrieved,
        "stored_acked" => stats.stored_acked,
        "nacks" => stats.nacks,
        "registers" => stats.registers,
        "unregisters" => stats.unregisters,
        "disconnect_reason" => error,
        );
        transition!(UnAuthDone(()))
    }
}

fn save_and_notify_undelivered_messages(
    webpush: &WebPushClient,
    srv: Rc<Server>,
    notifs: Vec<Notification>,
) {
    let srv2 = srv.clone();
    let uaid = webpush.uaid.clone();
    let connected_at = webpush.connected_at.clone();
    srv.handle.spawn(
        srv.ddb
            .store_messages(&webpush.uaid, &webpush.message_month, notifs)
            .and_then(move |_| {
                debug!("Finished saving unacked direct notifications, checking for reconnect");
                srv2.ddb.get_user(&uaid)
            })
            .and_then(move |user| {
                // Return an err to stop processing if the user hasn't reconnected yet, otherwise
                // attempt to construct a client to make the request
                if user.connected_at == connected_at {
                    future::err("No notify needed".into())
                } else if let Some(node_id) = user.node_id {
                    let result = AsyncClient::builder()
                        .timeout(Duration::from_secs(1))
                        .build();
                    if let Ok(client) = result {
                        future::ok((client, user.uaid, node_id))
                    } else {
                        future::err("Unable to build http client".into())
                    }
                } else {
                    future::err("No new node_id, notify not needed".into())
                }
            })
            .and_then(|(client, uaid, node_id)| {
                // Send the notify to the user
                let notify_url = format!("{}/notif/{}", node_id, uaid.to_simple());
                client
                    .put(&notify_url)
                    .send()
                    .map_err(|_| "Failed to send".into())
            })
            .then(|_| {
                debug!("Finished cleanup");
                Ok(())
            }),
    );
}

#[derive(StateMachineFuture)]
pub enum AuthClientState<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    #[state_machine_future(start, transitions(AwaitSend, DetermineAck))]
    Send {
        smessages: Vec<ServerMessage>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(DetermineAck, Send, AwaitDropUser))]
    AwaitSend {
        smessages: Vec<ServerMessage>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(
        transitions(IncrementStorage, CheckStorage, AwaitDropUser, AwaitMigrateUser, AwaitInput)
    )]
    DetermineAck { data: AuthClientData<T> },

    #[state_machine_future(
        transitions(DetermineAck, Send, AwaitInput, AwaitRegister, AwaitUnregister, AwaitDelete)
    )]
    AwaitInput { data: AuthClientData<T> },

    #[state_machine_future(transitions(AwaitIncrementStorage))]
    IncrementStorage { data: AuthClientData<T> },

    #[state_machine_future(transitions(DetermineAck))]
    AwaitIncrementStorage {
        response: MyFuture<UpdateItemOutput>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(AwaitCheckStorage))]
    CheckStorage { data: AuthClientData<T> },

    #[state_machine_future(transitions(Send, DetermineAck))]
    AwaitCheckStorage {
        response: MyFuture<CheckStorageResponse>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(DetermineAck))]
    AwaitMigrateUser {
        response: MyFuture<()>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(AuthDone))]
    AwaitDropUser {
        response: MyFuture<()>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(Send))]
    AwaitRegister {
        channel_id: Uuid,
        response: MyFuture<RegisterResponse>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(Send))]
    AwaitUnregister {
        channel_id: Uuid,
        code: u32,
        response: MyFuture<bool>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(transitions(DetermineAck))]
    AwaitDelete {
        response: MyFuture<()>,
        data: AuthClientData<T>,
    },

    #[state_machine_future(ready)]
    AuthDone(()),

    #[state_machine_future(error)]
    AuthClientStateError(Error),
}

impl<T> PollAuthClientState<T> for AuthClientState<T>
where
    T: Stream<Item = ClientMessage, Error = Error>
        + Sink<SinkItem = ServerMessage, SinkError = Error>
        + 'static,
{
    fn poll_send<'a>(send: &'a mut RentToOwn<'a, Send<T>>) -> Poll<AfterSend<T>, Error> {
        trace!("State: Send");
        let sent = {
            let Send {
                ref mut smessages,
                ref mut data,
                ..
            } = **send;
            if !smessages.is_empty() {
                let item = smessages.remove(0);
                let ret = data.ws.start_send(item).chain_err(|| "unable to send")?;
                match ret {
                    AsyncSink::Ready => true,
                    AsyncSink::NotReady(returned) => {
                        smessages.insert(0, returned);
                        return Ok(Async::NotReady);
                    }
                }
            } else {
                false
            }
        };

        let Send { smessages, data } = send.take();
        if sent {
            transition!(AwaitSend { smessages, data });
        }
        transition!(DetermineAck { data })
    }

    fn poll_await_send<'a>(
        await_send: &'a mut RentToOwn<'a, AwaitSend<T>>,
    ) -> Poll<AfterAwaitSend<T>, Error> {
        trace!("State: AwaitSend");
        try_ready!(await_send.data.ws.poll_complete());

        let AwaitSend { smessages, data } = await_send.take();
        let webpush_rc = data.webpush.clone();
        let webpush = webpush_rc.borrow();
        if webpush.sent_from_storage > data.srv.opts.msg_limit {
            // Exceeded the max limit of stored messages: drop the user to trigger a
            // re-register
            debug!("Dropping user: exceeded msg_limit");
            let response = Box::new(data.srv.ddb.drop_uaid(&webpush.uaid));
            transition!(AwaitDropUser { response, data });
        } else if !smessages.is_empty() {
            transition!(Send { smessages, data });
        }
        transition!(DetermineAck { data })
    }

    fn poll_determine_ack<'a>(
        detack: &'a mut RentToOwn<'a, DetermineAck<T>>,
    ) -> Poll<AfterDetermineAck<T>, Error> {
        let DetermineAck { data } = detack.take();
        let webpush_rc = data.webpush.clone();
        let webpush = webpush_rc.borrow();
        let all_acked = !webpush.unacked_messages();
        if all_acked && webpush.flags.check && webpush.flags.increment_storage {
            transition!(IncrementStorage { data });
        } else if all_acked && webpush.flags.check {
            transition!(CheckStorage { data });
        } else if all_acked && webpush.flags.rotate_message_table {
            debug!("Triggering migration");
            let response = Box::new(
                data.srv
                    .ddb
                    .migrate_user(&webpush.uaid, &webpush.message_month),
            );
            transition!(AwaitMigrateUser { response, data });
        } else if all_acked && webpush.flags.reset_uaid {
            debug!("Dropping user: flagged reset_uaid");
            let response = Box::new(data.srv.ddb.drop_uaid(&webpush.uaid));
            transition!(AwaitDropUser { response, data });
        }
        transition!(AwaitInput { data })
    }

    fn poll_await_input<'a>(
        await: &'a mut RentToOwn<'a, AwaitInput<T>>,
    ) -> Poll<AfterAwaitInput<T>, Error> {
        trace!("State: AwaitInput");
        let input = try_ready!(await.data.input_or_notif());
        let AwaitInput { data } = await.take();
        let webpush_rc = data.webpush.clone();
        let mut webpush = webpush_rc.borrow_mut();
        match input {
            Either::A(ClientMessage::Hello { .. }) => {
                Err(ErrorKind::InvalidStateTransition("AwaitInput".to_string(), "Hello".to_string()).into())
            }
            Either::A(ClientMessage::BroadcastSubscribe { broadcasts }) => {
                let broadcast_delta = {
                    let mut broadcast_subs = data.broadcast_subs.borrow_mut();
                    data.srv.process_broadcasts(
                        &mut broadcast_subs,
                        &Broadcast::from_hashmap(broadcasts),
                    )
                };

                if let Some(response) = broadcast_delta {
                    transition!(Send {
                        smessages: vec![ServerMessage::Broadcast {
                            broadcasts: response,
                        }],
                        data,
                    });
                } else {
                    transition!(AwaitInput { data });
                }
            }
            Either::A(ClientMessage::Register {
                channel_id: channel_id_str,
                key,
            }) => {
                debug!("Got a register command";
                       "channel_id" => &channel_id_str);
                let channel_id =
                    Uuid::parse_str(&channel_id_str).chain_err(|| "Invalid channelID")?;
                if channel_id.to_hyphenated().to_string() != channel_id_str {
                    return Err("Bad UUID format, use lower case, dashed format".into());
                }

                let uaid = webpush.uaid;
                let message_month = webpush.message_month.clone();
                let srv = data.srv.clone();
                let fut = data
                    .srv
                    .ddb
                    .register(&srv, &uaid, &channel_id, &message_month, key);
                transition!(AwaitRegister {
                    channel_id,
                    response: fut,
                    data,
                });
            }
            Either::A(ClientMessage::Unregister { channel_id, code }) => {
                debug!("Got a unregister command");
                // XXX: unregister should check the format of channel_id like
                // register does
                let uaid = webpush.uaid;
                let message_month = webpush.message_month.clone();
                let response =
                    Box::new(data.srv.ddb.unregister(&uaid, &channel_id, &message_month));
                transition!(AwaitUnregister {
                    channel_id,
                    code: code.unwrap_or(200),
                    response,
                    data,
                });
            }
            Either::A(ClientMessage::Nack { code, .. }) => {
                // only metric codes expected from the client (or 0)
                let mcode =
                    code.and_then(|code| {
                        if code >= 301 && code <= 303 {
                            Some(code)
                        } else {
                            None
                        }
                    }).unwrap_or(0);
                data.srv
                    .metrics
                    .incr_with_tags("ua.command.nack")
                    .with_tag("code", &mcode.to_string())
                    .send();
                webpush.stats.nacks += 1;
                transition!(AwaitInput { data });
            }
            Either::A(ClientMessage::Ack { updates }) => {
                data.srv.metrics.incr("ua.command.ack").ok();
                let mut fut: Option<MyFuture<()>> = None;
                for notif in &updates {
                    if let Some(pos) = webpush.unacked_direct_notifs.iter().position(|v| {
                        v.channel_id == notif.channel_id && v.version == notif.version
                    }) {
                        webpush.stats.direct_acked += 1;
                        webpush.unacked_direct_notifs.remove(pos);
                        continue;
                    };
                    if let Some(pos) = webpush.unacked_stored_notifs.iter().position(|v| {
                        v.channel_id == notif.channel_id && v.version == notif.version
                    }) {
                        webpush.stats.stored_acked += 1;
                        let message_month = webpush.message_month.clone();
                        let n = webpush.unacked_stored_notifs.remove(pos);
                        // Topic/legacy messages have no sortkey_timestamp
                        if n.sortkey_timestamp.is_none() {
                            fut = if let Some(call) = fut {
                                let my_fut =
                                    data.srv
                                        .ddb
                                        .delete_message(&message_month, &webpush.uaid, &n);
                                Some(Box::new(call.and_then(move |_| my_fut)))
                            } else {
                                Some(Box::new(data.srv.ddb.delete_message(
                                    &message_month,
                                    &webpush.uaid,
                                    &n,
                                )))
                            }
                        }
                        continue;
                    };
                }
                if let Some(my_fut) = fut {
                    transition!(AwaitDelete {
                        response: my_fut,
                        data,
                    });
                } else {
                    transition!(DetermineAck { data });
                }
            }
            Either::A(ClientMessage::Ping) => {
                // Clients shouldn't ping > than once per minute or we
                // disconnect them
                if sec_since_epoch() - webpush.last_ping >= 45 {
                    debug!("Got a ping, sending pong");
                    webpush.last_ping = sec_since_epoch();
                    transition!(Send {
                        smessages: vec![ServerMessage::Ping],
                        data,
                    })
                } else {
                    debug!("Got a ping too quickly, disconnecting");
                    Err(ErrorKind::ExcessivePing.into())
                }
            }
            Either::B(ServerNotification::Notification(notif)) => {
                if notif.ttl != 0 {
                    webpush.unacked_direct_notifs.push(notif.clone());
                }
                debug!("Got a notification to send, sending!");
                emit_metrics_for_send(&data.srv.metrics, &notif, "Direct");
                transition!(Send {
                    smessages: vec![ServerMessage::Notification(notif)],
                    data,
                });
            }
            Either::B(ServerNotification::CheckStorage) => {
                webpush.flags.include_topic = true;
                webpush.flags.check = true;
                transition!(DetermineAck { data });
            }
            Either::B(ServerNotification::Disconnect) => {
                debug!("Got told to disconnect, connecting client has our uaid");
                Err(ErrorKind::RepeatUaidDisconnect.into())
            }
        }
    }

    fn poll_increment_storage<'a>(
        increment_storage: &'a mut RentToOwn<'a, IncrementStorage<T>>,
    ) -> Poll<AfterIncrementStorage<T>, Error> {
        trace!("State: IncrementStorage");
        let webpush_rc = increment_storage.data.webpush.clone();
        let webpush = webpush_rc.borrow();
        let timestamp = webpush
            .unacked_stored_highest
            .ok_or("unacked_stored_highest unset")?
            .to_string();
        let response = Box::new(increment_storage.data.srv.ddb.increment_storage(
            &webpush.message_month,
            &webpush.uaid,
            &timestamp,
        ));
        transition!(AwaitIncrementStorage {
            response,
            data: increment_storage.take().data,
        })
    }

    fn poll_await_increment_storage<'a>(
        await_increment_storage: &'a mut RentToOwn<'a, AwaitIncrementStorage<T>>,
    ) -> Poll<AfterAwaitIncrementStorage<T>, Error> {
        trace!("State: AwaitIncrementStorage");
        try_ready!(await_increment_storage.response.poll());
        let AwaitIncrementStorage { data, .. } = await_increment_storage.take();
        let webpush = data.webpush.clone();
        webpush.borrow_mut().flags.increment_storage = false;
        transition!(DetermineAck { data })
    }

    fn poll_check_storage<'a>(
        check_storage: &'a mut RentToOwn<'a, CheckStorage<T>>,
    ) -> Poll<AfterCheckStorage<T>, Error> {
        trace!("State: CheckStorage");
        let CheckStorage { data } = check_storage.take();
        let response = Box::new({
            let webpush = data.webpush.borrow();
            data.srv.ddb.check_storage(
                &webpush.message_month.clone(),
                &webpush.uaid,
                webpush.flags.include_topic,
                webpush.unacked_stored_highest,
            )
        });
        transition!(AwaitCheckStorage { response, data })
    }

    fn poll_await_check_storage<'a>(
        await_check_storage: &'a mut RentToOwn<'a, AwaitCheckStorage<T>>,
    ) -> Poll<AfterAwaitCheckStorage<T>, Error> {
        trace!("State: AwaitCheckStorage");
        let (include_topic, mut messages, timestamp) =
            match try_ready!(await_check_storage.response.poll()) {
                CheckStorageResponse {
                    include_topic,
                    messages,
                    timestamp,
                } => (include_topic, messages, timestamp),
            };
        debug!("Got checkstorage response");

        let AwaitCheckStorage { data, .. } = await_check_storage.take();
        let webpush_rc = data.webpush.clone();
        let mut webpush = webpush_rc.borrow_mut();
        webpush.flags.include_topic = include_topic;
        debug!("Setting unacked stored highest to {:?}", timestamp);
        webpush.unacked_stored_highest = timestamp;
        if messages.is_empty() {
            webpush.flags.check = false;
            webpush.sent_from_storage = 0;
            transition!(DetermineAck { data });
        }

        // Filter out TTL expired messages
        let now = sec_since_epoch();
        let srv = data.srv.clone();
        messages = messages
            .into_iter()
            .filter_map(|n| {
                if !n.expired(now) {
                    return Some(n);
                }
                if n.sortkey_timestamp.is_none() {
                    srv.handle.spawn(
                        srv.ddb
                            .delete_message(&webpush.message_month, &webpush.uaid, &n)
                            .then(|_| {
                                debug!("Deleting expired message without sortkey_timestamp");
                                Ok(())
                            }),
                    );
                }
                None
            })
            .collect();
        webpush.flags.increment_storage = !include_topic && timestamp.is_some();
        // If there's still messages send them out
        if !messages.is_empty() {
            webpush
                .unacked_stored_notifs
                .extend(messages.iter().cloned());
            let smessages: Vec<_> = messages
                .into_iter()
                .inspect(|msg| emit_metrics_for_send(&data.srv.metrics, &msg, "Stored"))
                .map(ServerMessage::Notification)
                .collect();
            webpush.sent_from_storage += smessages.len() as u32;
            transition!(Send { smessages, data })
        } else {
            // No messages remaining
            transition!(DetermineAck { data })
        }
    }

    fn poll_await_migrate_user<'a>(
        await_migrate_user: &'a mut RentToOwn<'a, AwaitMigrateUser<T>>,
    ) -> Poll<AfterAwaitMigrateUser<T>, Error> {
        trace!("State: AwaitMigrateUser");
        try_ready!(await_migrate_user.response.poll());
        let AwaitMigrateUser { data, .. } = await_migrate_user.take();
        {
            let mut webpush = data.webpush.borrow_mut();
            webpush.message_month = data.srv.ddb.current_message_month.clone();
            webpush.flags.rotate_message_table = false;
        }
        transition!(DetermineAck { data })
    }

    fn poll_await_drop_user<'a>(
        await_drop_user: &'a mut RentToOwn<'a, AwaitDropUser<T>>,
    ) -> Poll<AfterAwaitDropUser, Error> {
        trace!("State: AwaitDropUser");
        try_ready!(await_drop_user.response.poll());
        transition!(AuthDone(()))
    }

    fn poll_await_register<'a>(
        await_register: &'a mut RentToOwn<'a, AwaitRegister<T>>,
    ) -> Poll<AfterAwaitRegister<T>, Error> {
        trace!("State: AwaitRegister");
        let msg = match try_ready!(await_register.response.poll()) {
            RegisterResponse::Success { endpoint } => {
                let mut webpush = await_register.data.webpush.borrow_mut();
                await_register
                    .data
                    .srv
                    .metrics
                    .incr("ua.command.register")
                    .ok();
                webpush.stats.registers += 1;
                ServerMessage::Register {
                    channel_id: await_register.channel_id,
                    status: 200,
                    push_endpoint: endpoint,
                }
            }
            RegisterResponse::Error { error_msg, status } => {
                debug!("Got unregister fail, error: {}", error_msg);
                ServerMessage::Register {
                    channel_id: await_register.channel_id,
                    status,
                    push_endpoint: "".into(),
                }
            }
        };

        transition!(Send {
            smessages: vec![msg],
            data: await_register.take().data,
        })
    }

    fn poll_await_unregister<'a>(
        await_unregister: &'a mut RentToOwn<'a, AwaitUnregister<T>>,
    ) -> Poll<AfterAwaitUnregister<T>, Error> {
        trace!("State: AwaitUnRegister");
        let msg = if try_ready!(await_unregister.response.poll()) {
            debug!("Got the unregister response");
            let mut webpush = await_unregister.data.webpush.borrow_mut();
            webpush.stats.unregisters += 1;
            ServerMessage::Unregister {
                channel_id: await_unregister.channel_id,
                status: 200,
            }
        } else {
            debug!("Got unregister fail");
            ServerMessage::Unregister {
                channel_id: await_unregister.channel_id,
                status: 500,
            }
        };

        let AwaitUnregister { code, data, .. } = await_unregister.take();
        data.srv
            .metrics
            .incr_with_tags("ua.command.unregister")
            .with_tag("code", &code.to_string())
            .send();
        transition!(Send {
            smessages: vec![msg],
            data,
        })
    }

    fn poll_await_delete<'a>(
        await_delete: &'a mut RentToOwn<'a, AwaitDelete<T>>,
    ) -> Poll<AfterAwaitDelete<T>, Error> {
        trace!("State: AwaitDelete");
        try_ready!(await_delete.response.poll());
        transition!(DetermineAck {
            data: await_delete.take().data,
        })
    }
}

fn emit_metrics_for_send(metrics: &StatsdClient, notif: &Notification, source: &'static str) {
    if notif.topic.is_some() {
        metrics.incr("ua.notification.topic").ok();
    }
    metrics
        .count_with_tags(
            "ua.message_data",
            notif.data.as_ref().map_or(0, |data| data.len() as i64),
        )
        .with_tag("source", source)
        .send();
}
