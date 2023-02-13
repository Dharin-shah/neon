//! This module implements the streaming side of replication protocol, starting
//! with the "START_REPLICATION" message.

use anyhow::Context as AnyhowContext;
use bytes::Bytes;

use pin_project_lite::pin_project;
use postgres_ffi::get_current_timestamp;
use postgres_ffi::{TimestampTz, MAX_SEND_SIZE};
use pq_proto::framed::ConnectionError;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::cmp::min;
use std::future::Future;
use std::io::ErrorKind;
use std::pin::Pin;

use std::sync::Arc;
use std::task::{ready, Context, Poll};
use std::time::Duration;
use std::{io, str};
use tokio::sync::watch::Receiver;
use tokio::time::timeout;
use tracing::*;
use utils::postgres_backend_async::QueryError;
use utils::send_rc::RefCellSend;
use utils::send_rc::SendRc;

use pq_proto::{BeMessage, FeMessage, ReplicationFeedback, WalSndKeepAlive, XLogDataBody};
use utils::{bin_ser::BeSer, lsn::Lsn, postgres_backend_async::PostgresBackend};

use crate::handler::SafekeeperPostgresHandler;
use crate::timeline::{ReplicaState, Timeline};
use crate::wal_storage::WalReader;
use crate::GlobalTimelines;

// See: https://www.postgresql.org/docs/13/protocol-replication.html
const HOT_STANDBY_FEEDBACK_TAG_BYTE: u8 = b'h';
const STANDBY_STATUS_UPDATE_TAG_BYTE: u8 = b'r';
// neon extension of replication protocol
const NEON_STATUS_UPDATE_TAG_BYTE: u8 = b'z';

type FullTransactionId = u64;

/// Hot standby feedback received from replica
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct HotStandbyFeedback {
    pub ts: TimestampTz,
    pub xmin: FullTransactionId,
    pub catalog_xmin: FullTransactionId,
}

impl HotStandbyFeedback {
    pub fn empty() -> HotStandbyFeedback {
        HotStandbyFeedback {
            ts: 0,
            xmin: 0,
            catalog_xmin: 0,
        }
    }
}

/// Standby status update
#[derive(Debug, Clone, Deserialize)]
pub struct StandbyReply {
    pub write_lsn: Lsn, // last lsn received by pageserver
    pub flush_lsn: Lsn, // pageserver's disk consistent lSN
    pub apply_lsn: Lsn, // pageserver's remote consistent lSN
    pub reply_ts: TimestampTz,
    pub reply_requested: bool,
}

/// Scope guard to unregister replication connection from timeline
struct ReplicationConnGuard {
    replica: usize, // replica internal ID assigned by timeline
    timeline: Arc<Timeline>,
}

impl Drop for ReplicationConnGuard {
    fn drop(&mut self) {
        self.timeline.remove_replica(self.replica);
    }
}

impl SafekeeperPostgresHandler {
    pub async fn handle_start_replication(
        &mut self,
        pgb: &mut PostgresBackend,
        start_pos: Lsn,
    ) -> Result<(), QueryError> {
        let appname = self.appname.clone();
        let tli = GlobalTimelines::get(self.ttid)?;

        let state = ReplicaState::new();
        // This replica_id is used below to check if it's time to stop replication.
        let replica_id = tli.add_replica(state);

        // Use a guard object to remove our entry from the timeline, when the background
        // thread and us have both finished using it.
        let _guard = Arc::new(ReplicationConnGuard {
            replica: replica_id,
            timeline: tli.clone(),
        });

        // Walproposer gets special handling: safekeeper must give proposer all
        // local WAL till the end, whether committed or not (walproposer will
        // hang otherwise). That's because walproposer runs the consensus and
        // synchronizes safekeepers on the most advanced one.
        //
        // There is a small risk of this WAL getting concurrently garbaged if
        // another compute rises which collects majority and starts fixing log
        // on this safekeeper itself. That's ok as (old) proposer will never be
        // able to commit such WAL.
        let stop_pos: Option<Lsn> = if self.is_walproposer_recovery() {
            let wal_end = tli.get_flush_lsn();
            Some(wal_end)
        } else {
            None
        };
        let end_pos = stop_pos.unwrap_or(Lsn::INVALID);

        info!(
            "starting streaming from {:?} till {:?}",
            start_pos, stop_pos
        );

        // switch to copy
        pgb.write_message_flush(&BeMessage::CopyBothResponse)
            .await?;

        let (_, persisted_state) = tli.get_state();
        let wal_reader = WalReader::new(
            self.conf.workdir.clone(),
            self.conf.timeline_dir(&tli.ttid),
            &persisted_state,
            start_pos,
            self.conf.wal_backup_enabled,
        )?;
        let write_ctx = SendRc::new(WriteContext {
            wal_reader: RefCell::new(wal_reader),
            send_buf: RefCell::new([0; MAX_SEND_SIZE]),
        });

        let mut c = ReplicationContext {
            tli,
            replica_id,
            appname,
            pgb,
            start_pos,
            end_pos,
            stop_pos,
            write_ctx,
            feedback: ReplicaState::new(),
        };

        let _phantom_wf = c.wait_wal_fut();
        let real_end_pos = c.end_pos;
        c.end_pos = c.start_pos + 1; // to well form read_wal future
        let _phantom_rf = c.read_wal_fut();
        c.end_pos = real_end_pos;

        ReplicationHandler {
            c,
            write_state: WriteWalState::Flush,
            _phantom_wf,
            _phantom_rf,
        }
        .await
    }
}

pin_project! {
    /// START_REPLICATION stream driver: sends WAL and receives feedback.
    struct ReplicationHandler<'a, WF, RF>
    where
        WF: Future<Output = anyhow::Result<Option<Lsn>>>,
        RF: Future<Output = anyhow::Result<usize>>,
    {
        c: ReplicationContext<'a>,
        #[pin]
        write_state: WriteWalState<WF, RF>,
        // To deduce anonymous types.
        _phantom_wf: WF,
        _phantom_rf: RF,
    }
}

/// Data ReplicationHandler maintains. Separated so we could generate WriteState
/// futures during init, deducing their type.
struct ReplicationContext<'a> {
    tli: Arc<Timeline>,
    appname: Option<String>,
    replica_id: usize,
    pgb: &'a mut PostgresBackend,
    // Position since which we are sending next chunk.
    start_pos: Lsn,
    // WAL up to this position is known to be locally available.
    end_pos: Lsn,
    // If present, terminate after reaching this position; used by walproposer
    // in recovery.
    stop_pos: Option<Lsn>,
    // This data is needed to create Future sending WAL, so we need to both have
    // it here (to create new future) and borrow it to the future itself.
    // Essentially this is a self referential struct. To satisfy borrow checker,
    // use Rc<RefCell>. To make ReplicationHandler itself Send'able future, wrap
    // it into SendRc; this is safe as ReplicationHandler is passed between
    // threads only as a whole (during rescheduling).
    //
    // Right now we're in CurrentThread runtime, so Send is somewhat redundant;
    // however, otherwise we'd need to inconveniently have separate !Send
    // version of pg backend Handler trait (and work with LocalSet).
    write_ctx: SendRc<WriteContext>,
    feedback: ReplicaState,
}

// State which ReplicationHandler needs to create futures sending data.
struct WriteContext {
    wal_reader: RefCell<WalReader>,
    // buffer for readling WAL into to send it
    send_buf: RefCell<[u8; MAX_SEND_SIZE]>,
}

// Yield points of WAL sending machinery.
pin_project! {
    #[project = WriteWalStateProj]
    enum WriteWalState<WF, RF>
    where
        WF: Future<Output = anyhow::Result<Option<Lsn>>>,
        RF: Future<Output = anyhow::Result<usize>>,
    {
        Wait{ #[pin] fut: WF},
        Read{ #[pin] fut: RF},
        Flush,
    }
}

impl<WF, RF> Future for ReplicationHandler<'_, WF, RF>
where
    WF: Future<Output = anyhow::Result<Option<Lsn>>>,
    RF: Future<Output = anyhow::Result<usize>>,
{
    type Output = Result<(), QueryError>;

    // We need to read feedback from the socket and write data there at the same
    // time. To avoid having to split socket, which creates messy split-join
    // APIs, is problematic with TLS [1] and needs to manage two tasks, just run
    // single task and use poll interfaces, basically manual state machine,
    // which is simple here.
    //
    // [1] https://github.com/tokio-rs/tls/issues/40
    //
    // Completes only when the stream is over, technically on error currently.
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(r) = self.as_mut().poll_read(cx) {
            return Poll::Ready(r);
        }
        self.as_mut().poll_write(cx)
    }
}

impl<WF, RF> ReplicationHandler<'_, WF, RF>
where
    WF: Future<Output = anyhow::Result<Option<Lsn>>>,
    RF: Future<Output = anyhow::Result<usize>>,
{
    // Poll reading, i.e. getting feedback and processing it. Completes only on error/end of stream.
    fn poll_read(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), QueryError>> {
        loop {
            match ready!(self.as_mut().project().c.pgb.poll_read_message(cx)) {
                Ok(Some(msg)) => self.as_mut().handle_feedback(&msg)?,
                Ok(None) => {
                    return Poll::Ready(Err(QueryError::from(ConnectionError::Io(io::Error::new(
                        ErrorKind::Other,
                        "EOF on START_WAL_PUSH stream",
                    )))))
                }
                Err(err) => return Poll::Ready(Err(err.into())),
            };
        }
    }

    fn handle_feedback(self: Pin<&mut Self>, msg: &FeMessage) -> Result<(), QueryError> {
        let this = self.project();
        match &msg {
            FeMessage::CopyData(m) => {
                // There's three possible data messages that the client is supposed to send here:
                // `HotStandbyFeedback` and `StandbyStatusUpdate` and `NeonStandbyFeedback`.
                match m.first().cloned() {
                    Some(HOT_STANDBY_FEEDBACK_TAG_BYTE) => {
                        // Note: deserializing is on m[1..] because we skip the tag byte.
                        this.c.feedback.hs_feedback = HotStandbyFeedback::des(&m[1..])
                            .context("failed to deserialize HotStandbyFeedback")?;
                        this.c
                            .tli
                            .update_replica_state(this.c.replica_id, this.c.feedback);
                    }
                    Some(STANDBY_STATUS_UPDATE_TAG_BYTE) => {
                        let _reply = StandbyReply::des(&m[1..])
                            .context("failed to deserialize StandbyReply")?;
                        // This must be a regular postgres replica,
                        // because pageserver doesn't send this type of messages to safekeeper.
                        // Currently we just ignore this, tracking progress for them is not supported.
                    }
                    Some(NEON_STATUS_UPDATE_TAG_BYTE) => {
                        // Note: deserializing is on m[9..] because we skip the tag byte and len bytes.
                        let buf = Bytes::copy_from_slice(&m[9..]);
                        let reply = ReplicationFeedback::parse(buf);

                        trace!("ReplicationFeedback is {:?}", reply);
                        // Only pageserver sends ReplicationFeedback, so set the flag.
                        // This replica is the source of information to resend to compute.
                        this.c.feedback.pageserver_feedback = Some(reply);

                        this.c
                            .tli
                            .update_replica_state(this.c.replica_id, this.c.feedback);
                    }
                    _ => warn!("unexpected message {:?}", msg),
                }
            }
            FeMessage::CopyFail => {
                // XXX we should probably (tell pgb to) close the socket, as
                // CopyFail in duplex copy is somewhat unexpected (at least to
                // PG walsender; evidently client should finish it with
                // CopyDone). Note that sync rust-postgres client (which we
                // don't use anymore) hangs otherwise.
                // https://github.com/sfackler/rust-postgres/issues/755
                // https://github.com/neondatabase/neon/issues/935
                //
                return Err(anyhow::anyhow!("unexpected CopyFail").into());
            }
            _ => {
                return Err(
                    anyhow::anyhow!("unexpected message {:?} in replication stream", msg).into(),
                );
            }
        };
        Ok(())
    }

    // Poll writing, i.e. sending more WAL. Completes only on error or when we
    // decide to shutdown connection -- receiver is caughtup and there is no
    // active computes; this is still handled as Err though.
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Result<(), QueryError>> {
        // send while we don't block or error out
        loop {
            match &mut self.as_mut().project().write_state.project() {
                WriteWalStateProj::Wait { fut } => match ready!(fut.as_mut().poll(cx))? {
                    Some(lsn) => {
                        self.as_mut().project().c.end_pos = lsn;
                        self.as_mut().start_read_wal();
                        continue;
                    }
                    // Timed out waiting for WAL, send keepalive and possibly terminate.
                    None => {
                        let mut this = self.as_mut().project();
                        if this.c.tli.should_walsender_stop(this.c.replica_id) {
                            // Terminate if there is nothing more to send.
                            // TODO close the stream properly
                            return Poll::Ready(Err(anyhow::anyhow!(format!(
                                "ending streaming to {:?} at {}, receiver is caughtup and there is no computes",
                                self.c.appname, self.c.start_pos,
                            )).into()));
                        }
                        this.c
                            .pgb
                            .write_message(&BeMessage::KeepAlive(WalSndKeepAlive {
                                sent_ptr: this.c.end_pos.0,
                                timestamp: get_current_timestamp(),
                                request_reply: true,
                            }))?;
                        /* flush KA */
                        this.write_state.set(WriteWalState::Flush);
                    }
                },
                WriteWalStateProj::Read { fut } => {
                    let read_len = ready!(fut.as_mut().poll(cx))?;
                    assert!(read_len > 0, "read_len={}", read_len);

                    let mut this = self.as_mut().project();
                    let write_ctx_clone = this.c.write_ctx.clone();
                    let send_buf = &write_ctx_clone.send_buf.borrow()[..read_len];
                    let chunk_end = this.c.start_pos + read_len as u64;
                    // write data to the output buffer
                    this.c
                        .pgb
                        .write_message(&BeMessage::XLogData(XLogDataBody {
                            wal_start: this.c.start_pos.0,
                            wal_end: this.c.end_pos.0,
                            timestamp: get_current_timestamp(),
                            data: send_buf,
                        }))
                        .context("Failed to write XLogData")?;
                    trace!("wrote a chunk of wal {}-{}", this.c.start_pos, chunk_end);
                    this.c.start_pos = chunk_end;
                    // and flush it
                    this.write_state.set(WriteWalState::Flush);
                }
                WriteWalStateProj::Flush => {
                    let this = self.as_mut().project();

                    ready!(this.c.pgb.poll_flush(cx))?;
                    // If we are streaming to walproposer, check it is time to stop.
                    if let Some(stop_pos) = this.c.stop_pos {
                        if this.c.start_pos >= stop_pos {
                            // recovery finished
                            // TODO close the stream properly
                            return Poll::Ready(Err(anyhow::anyhow!(format!(
                                "ending streaming to walproposer at {}, receiver is caughtup and there is no computes",
                                this.c.start_pos)).into()));
                        }
                        self.as_mut().start_read_wal();
                        continue;
                    } else {
                        // if we don't know next portion is already available, wait
                        // for it; otherwise proceed to sending
                        if self.c.end_pos <= self.c.start_pos {
                            self.as_mut().start_wait_wal();
                        } else {
                            self.as_mut().start_read_wal();
                        }
                    }
                }
            }
        }
    }

    // Start waiting for WAL, creating future doing that.
    fn start_wait_wal(self: Pin<&mut Self>) {
        let fut = self.c.wait_wal_fut();
        self.project().write_state.set(WriteWalState::Wait {
            fut: {
                // SAFETY: this function is the only way to assign WaitWal to
                // write_state. We just workaround impossibility of specifying
                // async fn type, which is anonymous.
                // transmute_copy is used as transmute refuses generic param:
                // https://users.rust-lang.org/t/transmute-doesnt-work-on-generic-types/87272
                assert_eq!(std::mem::size_of::<WF>(), std::mem::size_of_val(&fut));
                let t = unsafe { std::mem::transmute_copy(&fut) };
                std::mem::forget(fut);
                t
            },
        });
    }

    // Switch into reading WAL state, creating Future doing that.
    fn start_read_wal(self: Pin<&mut Self>) {
        let fut = self.c.read_wal_fut();
        self.project().write_state.set(WriteWalState::Read {
            fut: {
                // SAFETY: this function is the only way to assign ReadWal to
                // write_state. We just workaround impossibility of specifying
                // async fn type, which is anonymous.
                // transmute_copy is used as transmute refuses generic param:
                // https://users.rust-lang.org/t/transmute-doesnt-work-on-generic-types/87272
                assert_eq!(std::mem::size_of::<RF>(), std::mem::size_of_val(&fut));
                let t = unsafe { std::mem::transmute_copy(&fut) };
                std::mem::forget(fut);
                t
            },
        });
    }
}

impl ReplicationContext<'_> {
    // Create future waiting for WAL.
    fn wait_wal_fut(&self) -> impl Future<Output = anyhow::Result<Option<Lsn>>> {
        let mut commit_lsn_watch_rx = self.tli.get_commit_lsn_watch_rx();
        let start_pos = self.start_pos;
        async move { wait_for_lsn(&mut commit_lsn_watch_rx, start_pos).await }
    }

    // Create future reading WAL.
    fn read_wal_fut(&self) -> impl Future<Output = anyhow::Result<usize>> {
        let mut send_size = self
            .end_pos
            .checked_sub(self.start_pos)
            .expect("reading wal without waiting for it first")
            .0 as usize;
        send_size = min(send_size, self.write_ctx.send_buf.borrow().len());
        let write_ctx_fut = self.write_ctx.clone();
        async move {
            let mut wal_reader_ref = write_ctx_fut.wal_reader.borrow_mut_send();
            let mut send_buf_ref = write_ctx_fut.send_buf.borrow_mut_send();

            let send_buf = &mut send_buf_ref[..send_size];
            wal_reader_ref.read(send_buf).await
        }
    }
}

const POLL_STATE_TIMEOUT: Duration = Duration::from_secs(1);

// Wait until we have commit_lsn > lsn or timeout expires. Returns
// - Ok(Some(commit_lsn)) if needed lsn is successfully observed;
// - Ok(None) if timeout expired;
// - Err in case of error (if watch channel is in trouble, shouldn't happen).
async fn wait_for_lsn(rx: &mut Receiver<Lsn>, lsn: Lsn) -> anyhow::Result<Option<Lsn>> {
    let commit_lsn: Lsn = *rx.borrow();
    if commit_lsn > lsn {
        return Ok(Some(commit_lsn));
    }

    let res = timeout(POLL_STATE_TIMEOUT, async move {
        let mut commit_lsn;
        loop {
            rx.changed().await?;
            commit_lsn = *rx.borrow();
            if commit_lsn > lsn {
                break;
            }
        }

        Ok(commit_lsn)
    })
    .await;

    match res {
        // success
        Ok(Ok(commit_lsn)) => Ok(Some(commit_lsn)),
        // error inside closure
        Ok(Err(err)) => Err(err),
        // timeout
        Err(_) => Ok(None),
    }
}
