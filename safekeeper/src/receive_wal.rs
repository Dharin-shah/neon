//! Safekeeper communication endpoint to WAL proposer (compute node).
//! Gets messages from the network, passes them down to consensus module and
//! sends replies back.

use anyhow::{anyhow, Context};
use bytes::BytesMut;
use pq_proto::framed::ConnectionError;
use std::io;
use std::io::ErrorKind;
use std::net::SocketAddr;
use std::sync::Arc;
use std::thread;
use std::thread::JoinHandle;
use tokio::sync::mpsc::channel;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Receiver;
use tokio::sync::mpsc::Sender;
use tracing::*;

use crate::handler::SafekeeperPostgresHandler;
use crate::safekeeper::AcceptorProposerMessage;
use crate::safekeeper::ProposerAcceptorMessage;
use crate::safekeeper::ServerInfo;
use crate::timeline::Timeline;
use crate::GlobalTimelines;
use pq_proto::BeMessage;
use pq_proto::FeMessage;
use utils::id::TenantTimelineId;
use utils::lsn::Lsn;
use utils::postgres_backend_async::PostgresBackend;
use utils::postgres_backend_async::PostgresBackendReader;
use utils::postgres_backend_async::QueryError;

const MSG_QUEUE_SIZE: usize = 256;
const REPLY_QUEUE_SIZE: usize = 16;

impl SafekeeperPostgresHandler {
    pub async fn handle_start_wal_push(
        &mut self,
        pgb: &mut PostgresBackend,
    ) -> Result<(), QueryError> {
        // Notify the libpq client that it's allowed to send `CopyData` messages
        pgb.write_message_flush(&BeMessage::CopyBothResponse)
            .await?;

        // Experiments confirm that doing network IO in one (this) thread and
        // processing with disc IO in another significantly improves
        // performance; we spawn off WalAcceptor thread for message processing
        // to this end.
        let (msg_tx, msg_rx) = channel(MSG_QUEUE_SIZE);
        let (reply_tx, reply_rx) = channel(REPLY_QUEUE_SIZE);
        let mut acceptor_handle: Option<JoinHandle<anyhow::Result<()>>> = None;

        // Concurrently receive and send data; replies are not synchronized with
        // sends, so this avoids deadlocks.
        let mut pgb_reader = pgb.split().context("START_WAL_PUSH split")?;
        let peer_addr = *pgb.get_peer_addr();
        let res = tokio::select! {
            // todo: add read|write .context to these errors
            r = read_network(self.ttid, &mut pgb_reader, peer_addr, msg_tx, &mut acceptor_handle, msg_rx, reply_tx) => r,
            r = write_network(pgb, reply_rx) => r,
        };

        // Join pg backend back.
        pgb.unsplit(pgb_reader)?;

        // Join the spawned WalAcceptor. At this point chans to/from it passed
        // to network routines are dropped, so it will exit as soon as it
        // touches them.
        match acceptor_handle {
            None => {
                // failed even before spawning; read_network should have error
                Err(res.expect_err("no error with WalAcceptor not spawn"))
            }
            Some(handle) => {
                let wal_acceptor_res = handle.join();

                // If there was any network error, return it.
                res?;

                // Otherwise, WalAcceptor thread must have errored.
                match wal_acceptor_res {
                    Ok(Ok(_)) => Ok(()), // can't happen currently; would be if we add graceful termination
                    Ok(Err(e)) => Err(QueryError::Other(e.context("WAL acceptor"))),
                    Err(_) => Err(QueryError::Other(anyhow!("WalAcceptor thread panicked",))),
                }
            }
        }
    }
}

/// Read next message from walproposer.
/// TODO: Return Ok(None) on graceful termination.
async fn read_message(
    pgb_reader: &mut PostgresBackendReader,
) -> Result<ProposerAcceptorMessage, QueryError> {
    let copy_data = match pgb_reader.read_message().await {
        Ok(Some(FeMessage::CopyData(bytes))) => bytes,
        Ok(Some(msg)) => {
            return Err(QueryError::Other(anyhow::anyhow!(
                "expected `CopyData` message, found {msg:?}"
            )))
        }
        Ok(None) => {
            return Err(QueryError::from(ConnectionError::Io(io::Error::new(
                ErrorKind::Other,
                "EOF on START_WAL_PUSH stream",
            ))))
        }
        Err(e) => return Err(QueryError::from(e)),
    };

    let msg = ProposerAcceptorMessage::parse(copy_data)?;
    Ok(msg)
}

/// Read messages from socket and pass it to WalAcceptor thread. Returns Ok(())
/// if msg_tx closed; it must mean WalAcceptor terminated, joining it should
/// tell the error.
async fn read_network(
    ttid: TenantTimelineId,
    pgb_reader: &mut PostgresBackendReader,
    peer_addr: SocketAddr,
    msg_tx: Sender<ProposerAcceptorMessage>,
    // WalAcceptor is spawned when we learn server info from walproposer and
    // create timeline; handle is put here.
    acceptor_handle: &mut Option<JoinHandle<anyhow::Result<()>>>,
    msg_rx: Receiver<ProposerAcceptorMessage>,
    reply_tx: Sender<AcceptorProposerMessage>,
) -> Result<(), QueryError> {
    // Receive information about server to create timeline, if not yet.
    let next_msg = read_message(pgb_reader).await?;
    let tli = match next_msg {
        ProposerAcceptorMessage::Greeting(ref greeting) => {
            info!(
                "start handshake with walproposer {} sysid {} timeline {}",
                peer_addr, greeting.system_id, greeting.tli,
            );
            let server_info = ServerInfo {
                pg_version: greeting.pg_version,
                system_id: greeting.system_id,
                wal_seg_size: greeting.wal_seg_size,
            };
            GlobalTimelines::create(ttid, server_info, Lsn::INVALID, Lsn::INVALID).await?
        }
        _ => {
            return Err(QueryError::Other(anyhow::anyhow!(
                "unexpected message {next_msg:?} instead of greeting"
            )))
        }
    };
    // Now that we have timeline, register the connection.
    tli.on_compute_connect().await?;

    *acceptor_handle = Some(
        WalAcceptor::spawn(tli.clone(), msg_rx, reply_tx).context("spawn WalAcceptor thread")?,
    );

    // Forward all messages to WalAcceptor
    let res = read_network_loop(pgb_reader, msg_tx, next_msg).await;
    // Unregister connection. XXX this is much more suitable for Drop, but async
    // Drop doesn't exist, and spawning task in thread-per-conn model is bad, as
    // thread local executor might be gone before task finishes.
    if let Err(e) = tli.on_compute_disconnect().await {
        error!("failed to unregister compute connection: {}", e);
    }
    res
}

async fn read_network_loop(
    pgb_reader: &mut PostgresBackendReader,
    msg_tx: Sender<ProposerAcceptorMessage>,
    mut next_msg: ProposerAcceptorMessage,
) -> Result<(), QueryError> {
    loop {
        if msg_tx.send(next_msg).await.is_err() {
            return Ok(()); // chan closed, WalAcceptor terminated
        }
        next_msg = read_message(pgb_reader).await?;
    }
}

/// Read replies from WalAcceptor and pass them back to socket. Returns Ok(())
/// if reply_rx closed; it must mean WalAcceptor terminated, joining it should
/// tell the error.
async fn write_network(
    pgb_writer: &mut PostgresBackend,
    mut reply_rx: Receiver<AcceptorProposerMessage>,
) -> Result<(), QueryError> {
    let mut buf = BytesMut::with_capacity(128);

    loop {
        match reply_rx.recv().await {
            Some(msg) => {
                buf.clear();
                msg.serialize(&mut buf)?;
                pgb_writer
                    .write_message_flush(&BeMessage::CopyData(&buf))
                    .await?;
            }
            None => return Ok(()), // chan closed, WalAcceptor terminated
        }
    }
}

/// Takes messages from msg_rx, processes and pushes replies to reply_tx.
struct WalAcceptor {
    tli: Arc<Timeline>,
    msg_rx: Receiver<ProposerAcceptorMessage>,
    reply_tx: Sender<AcceptorProposerMessage>,
}

impl WalAcceptor {
    /// Spawn thread with WalAcceptor running, return handle to it.
    fn spawn(
        tli: Arc<Timeline>,
        msg_rx: Receiver<ProposerAcceptorMessage>,
        reply_tx: Sender<AcceptorProposerMessage>,
    ) -> anyhow::Result<JoinHandle<anyhow::Result<()>>> {
        let thread_name = format!("WAL acceptor {}", tli.ttid);
        thread::Builder::new()
            .name(thread_name)
            .spawn(move || -> anyhow::Result<()> {
                let mut wa = WalAcceptor {
                    tli,
                    msg_rx,
                    reply_tx,
                };

                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()?;

                let span_ttid = wa.tli.ttid; // satisfy borrow checker
                runtime.block_on(
                    wa.run()
                        .instrument(info_span!("WAL acceptor", ttid = %span_ttid)),
                )
            })
            .map_err(anyhow::Error::from)
    }

    /// The main loop. Returns Ok(()) if either msg_rx or reply_tx got closed;
    /// it must mean that network thread terminated.
    async fn run(&mut self) -> anyhow::Result<()> {
        let mut next_msg: ProposerAcceptorMessage;

        loop {
            let opt_msg = self.msg_rx.recv().await;
            if opt_msg.is_none() {
                return Ok(()); // chan closed, streaming terminated
            }
            next_msg = opt_msg.unwrap();

            if matches!(next_msg, ProposerAcceptorMessage::AppendRequest(_)) {
                // loop through AppendRequest's while it's readily available to
                // write as many WAL as possible without fsyncing
                while let ProposerAcceptorMessage::AppendRequest(append_request) = next_msg {
                    let noflush_msg = ProposerAcceptorMessage::NoFlushAppendRequest(append_request);

                    if let Some(reply) = self.tli.process_msg(&noflush_msg)? {
                        if self.reply_tx.send(reply).await.is_err() {
                            return Ok(()); // chan closed, streaming terminated
                        }
                    }

                    match self.msg_rx.try_recv() {
                        Ok(msg) => next_msg = msg,
                        Err(TryRecvError::Empty) => break,
                        Err(TryRecvError::Disconnected) => return Ok(()), // chan closed, streaming terminated
                    }
                }

                // flush all written WAL to the disk
                if let Some(reply) = self.tli.process_msg(&ProposerAcceptorMessage::FlushWAL)? {
                    if self.reply_tx.send(reply).await.is_err() {
                        return Ok(()); // chan closed, streaming terminated
                    }
                }
            } else {
                // process message other than AppendRequest
                if let Some(reply) = self.tli.process_msg(&next_msg)? {
                    if self.reply_tx.send(reply).await.is_err() {
                        return Ok(()); // chan closed, streaming terminated
                    }
                }
            }
        }
    }
}
