//! The timestamp collector collects liveness information from peers.

use std::{net::SocketAddr, sync::Arc};

use thiserror::Error;
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tracing::{Level, Span};

use crate::{
    address_book::AddressMetrics, meta_addr::MetaAddrChange, AddressBook, BoxError, Config,
};

/// The `AddressBookUpdater` hooks into incoming message streams for each peer
/// and lets the owner of the sender handle update the address book. For
/// example, it can be used to record per-connection last-seen timestamps, or
/// add new initial peers to the address book.
#[derive(Debug, Eq, PartialEq)]
pub struct AddressBookUpdater;

#[derive(Copy, Clone, Debug, Error, Eq, PartialEq, Hash)]
#[error("all address book updater senders are closed")]
pub struct AllAddressBookUpdaterSendersClosed;

impl AddressBookUpdater {
    /// Spawn a new [`AddressBookUpdater`] task, updating a new [`AddressBook`]
    /// configured with Zebra's actual `local_listener` address.
    ///
    /// Returns handles for:
    /// - the address book,
    /// - the transmission channel for address book update events,
    /// - a watch channel for address book metrics, and
    /// - the address book updater task join handle.
    ///
    /// The task exits with an error when the returned [`mpsc::Sender`] is closed.
    pub fn spawn(
        config: &Config,
        local_listener: SocketAddr,
    ) -> (
        Arc<std::sync::Mutex<AddressBook>>,
        mpsc::Sender<MetaAddrChange>,
        watch::Receiver<AddressMetrics>,
        JoinHandle<Result<(), BoxError>>,
    ) {
        // Create an mpsc channel for peerset address book updates,
        // based on the maximum number of inbound and outbound peers.
        let (worker_tx, mut worker_rx) = mpsc::channel(config.peerset_total_connection_limit());

        let address_book = AddressBook::new(
            local_listener,
            config.network,
            span!(Level::TRACE, "address book"),
        );
        let address_metrics = address_book.address_metrics_watcher();
        let address_book = Arc::new(std::sync::Mutex::new(address_book));

        #[cfg(feature = "progress-bar")]
        let address_bar = howudoin::new().label("Known Peer Addresses");
        #[cfg(feature = "progress-bar")]
        let mut address_info = address_metrics.clone();

        let worker_address_book = address_book.clone();
        let worker = move || {
            info!("starting the address book updater");

            while let Some(event) = worker_rx.blocking_recv() {
                trace!(?event, "got address book change");

                // # Correctness
                //
                // Briefly hold the address book threaded mutex, to update the
                // state for a single address.
                worker_address_book
                    .lock()
                    .expect("mutex should be unpoisoned")
                    .update(event);

                #[cfg(feature = "progress-bar")]
                if address_info.has_changed()? {
                    let address_info = *address_info.borrow_and_update();

                    address_bar
                        .set_pos(u64::try_from(address_info.addresses).expect("fits in u64"))
                        .set_len(u64::try_from(address_info.address_limit).expect("fits in u64"));
                }
            }

            let error = Err(AllAddressBookUpdaterSendersClosed.into());
            info!(?error, "stopping address book updater");
            error
        };

        // Correctness: spawn address book accesses on a blocking thread,
        //              to avoid deadlocks (see #1976)
        let span = Span::current();
        let address_book_updater_task_handle =
            tokio::task::spawn_blocking(move || span.in_scope(worker));

        (
            address_book,
            worker_tx,
            address_metrics,
            address_book_updater_task_handle,
        )
    }
}
