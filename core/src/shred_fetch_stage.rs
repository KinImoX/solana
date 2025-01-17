//! The `shred_fetch_stage` pulls shreds from UDP sockets and sends it to a channel.

use {
    crate::packet_hasher::PacketHasher,
    crossbeam_channel::{unbounded, Sender},
    lru::LruCache,
    solana_ledger::shred::{should_discard_shred, ShredFetchStats},
    solana_perf::packet::{Packet, PacketBatch, PacketBatchRecycler, PacketFlags},
    solana_runtime::bank_forks::BankForks,
    solana_sdk::clock::{Slot, DEFAULT_MS_PER_SLOT},
    solana_streamer::streamer::{self, PacketBatchReceiver, StreamerReceiveStats},
    std::{
        net::UdpSocket,
        sync::{atomic::AtomicBool, Arc, RwLock},
        thread::{self, Builder, JoinHandle},
        time::{Duration, Instant},
    },
};

const DEFAULT_LRU_SIZE: usize = 10_000;
type ShredsReceived = LruCache<u64, ()>;

pub(crate) struct ShredFetchStage {
    thread_hdls: Vec<JoinHandle<()>>,
}

impl ShredFetchStage {
    // updates packets received on a channel and sends them on another channel
    fn modify_packets(
        recvr: PacketBatchReceiver,
        sendr: Sender<Vec<PacketBatch>>,
        bank_forks: &RwLock<BankForks>,
        shred_version: u16,
        name: &'static str,
        flags: PacketFlags,
    ) {
        const STATS_SUBMIT_CADENCE: Duration = Duration::from_secs(1);
        let mut shreds_received = LruCache::new(DEFAULT_LRU_SIZE);
        let mut last_updated = Instant::now();

        // In the case of bank_forks=None, setup to accept any slot range
        let mut last_root = 0;
        let mut last_slot = std::u64::MAX;
        let mut slots_per_epoch = 0;

        let mut stats = ShredFetchStats::default();
        let mut packet_hasher = PacketHasher::default();

        while let Some(mut packet_batch) = recvr.iter().next() {
            if last_updated.elapsed().as_millis() as u64 > DEFAULT_MS_PER_SLOT {
                last_updated = Instant::now();
                packet_hasher.reset();
                shreds_received.clear();
                {
                    let bank_forks_r = bank_forks.read().unwrap();
                    last_root = bank_forks_r.root();
                    let working_bank = bank_forks_r.working_bank();
                    last_slot = working_bank.slot();
                    let root_bank = bank_forks_r.root_bank();
                    slots_per_epoch = root_bank.get_slots_in_epoch(root_bank.epoch());
                }
            }
            stats.shred_count += packet_batch.len();
            // Limit shreds to 2 epochs away.
            let max_slot = last_slot + 2 * slots_per_epoch;
            for packet in packet_batch.iter_mut() {
                if should_discard_packet(
                    packet,
                    last_root,
                    max_slot,
                    shred_version,
                    &packet_hasher,
                    &mut shreds_received,
                    &mut stats,
                ) {
                    packet.meta.set_discard(true);
                } else {
                    packet.meta.flags.insert(flags);
                }
            }
            stats.maybe_submit(name, STATS_SUBMIT_CADENCE);
            if sendr.send(vec![packet_batch]).is_err() {
                break;
            }
        }
    }

    fn packet_modifier(
        sockets: Vec<Arc<UdpSocket>>,
        exit: &Arc<AtomicBool>,
        sender: Sender<Vec<PacketBatch>>,
        recycler: PacketBatchRecycler,
        bank_forks: Arc<RwLock<BankForks>>,
        shred_version: u16,
        name: &'static str,
        flags: PacketFlags,
    ) -> (Vec<JoinHandle<()>>, JoinHandle<()>) {
        let (packet_sender, packet_receiver) = unbounded();
        let streamers = sockets
            .into_iter()
            .map(|s| {
                streamer::receiver(
                    s,
                    exit.clone(),
                    packet_sender.clone(),
                    recycler.clone(),
                    Arc::new(StreamerReceiveStats::new("packet_modifier")),
                    1,
                    true,
                    None,
                )
            })
            .collect();

        let modifier_hdl = Builder::new()
            .name("solana-tvu-fetch-stage-packet-modifier".to_string())
            .spawn(move || {
                Self::modify_packets(
                    packet_receiver,
                    sender,
                    &bank_forks,
                    shred_version,
                    name,
                    flags,
                )
            })
            .unwrap();
        (streamers, modifier_hdl)
    }

    pub(crate) fn new(
        sockets: Vec<Arc<UdpSocket>>,
        forward_sockets: Vec<Arc<UdpSocket>>,
        repair_socket: Arc<UdpSocket>,
        sender: Sender<Vec<PacketBatch>>,
        shred_version: u16,
        bank_forks: Arc<RwLock<BankForks>>,
        exit: &Arc<AtomicBool>,
    ) -> Self {
        let recycler = PacketBatchRecycler::warmed(100, 1024);

        let (mut tvu_threads, tvu_filter) = Self::packet_modifier(
            sockets,
            exit,
            sender.clone(),
            recycler.clone(),
            bank_forks.clone(),
            shred_version,
            "shred_fetch",
            PacketFlags::empty(),
        );

        let (tvu_forwards_threads, fwd_thread_hdl) = Self::packet_modifier(
            forward_sockets,
            exit,
            sender.clone(),
            recycler.clone(),
            bank_forks.clone(),
            shred_version,
            "shred_fetch_tvu_forwards",
            PacketFlags::FORWARDED,
        );

        let (repair_receiver, repair_handler) = Self::packet_modifier(
            vec![repair_socket],
            exit,
            sender,
            recycler,
            bank_forks,
            shred_version,
            "shred_fetch_repair",
            PacketFlags::REPAIR,
        );

        tvu_threads.extend(tvu_forwards_threads.into_iter());
        tvu_threads.extend(repair_receiver.into_iter());
        tvu_threads.push(tvu_filter);
        tvu_threads.push(fwd_thread_hdl);
        tvu_threads.push(repair_handler);

        Self {
            thread_hdls: tvu_threads,
        }
    }

    pub(crate) fn join(self) -> thread::Result<()> {
        for thread_hdl in self.thread_hdls {
            thread_hdl.join()?;
        }
        Ok(())
    }
}

// Returns true if the packet should be marked as discard.
#[must_use]
fn should_discard_packet(
    packet: &Packet,
    root: Slot,
    max_slot: Slot, // Max slot to ingest shreds for.
    shred_version: u16,
    packet_hasher: &PacketHasher,
    shreds_received: &mut ShredsReceived,
    stats: &mut ShredFetchStats,
) -> bool {
    if should_discard_shred(packet, root, max_slot, shred_version, stats) {
        return true;
    }
    let hash = packet_hasher.hash_packet(packet);
    match shreds_received.put(hash, ()) {
        None => false,
        Some(()) => {
            stats.duplicate_shred += 1;
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        solana_ledger::{
            blockstore::MAX_DATA_SHREDS_PER_SLOT,
            shred::{Shred, ShredFlags},
        },
    };

    #[test]
    fn test_data_code_same_index() {
        solana_logger::setup();
        let mut shreds_received = LruCache::new(DEFAULT_LRU_SIZE);
        let mut packet = Packet::default();
        let mut stats = ShredFetchStats::default();

        let slot = 2;
        let shred_version = 45189;
        let shred = Shred::new_from_data(
            slot,
            3,   // shred index
            1,   // parent offset
            &[], // data
            ShredFlags::LAST_SHRED_IN_SLOT,
            0, // reference_tick
            shred_version,
            3, // fec_set_index
        );
        shred.copy_to_packet(&mut packet);

        let hasher = PacketHasher::default();

        let last_root = 0;
        let last_slot = 100;
        let slots_per_epoch = 10;
        let max_slot = last_slot + 2 * slots_per_epoch;
        assert!(!should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
        let coding = solana_ledger::shred::Shredder::generate_coding_shreds(
            &[shred],
            false, // is_last_in_slot
            3,     // next_code_index
        );
        coding[0].copy_to_packet(&mut packet);
        assert!(!should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
    }

    #[test]
    fn test_shred_filter() {
        solana_logger::setup();
        let mut shreds_received = LruCache::new(DEFAULT_LRU_SIZE);
        let mut packet = Packet::default();
        let mut stats = ShredFetchStats::default();
        let last_root = 0;
        let last_slot = 100;
        let slots_per_epoch = 10;
        let shred_version = 59445;
        let max_slot = last_slot + 2 * slots_per_epoch;

        let hasher = PacketHasher::default();

        // packet size is 0, so cannot get index
        assert!(should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
        assert_eq!(stats.index_overrun, 1);
        let shred = Shred::new_from_data(
            2,   // slot
            3,   // index
            1,   // parent_offset
            &[], // data
            ShredFlags::LAST_SHRED_IN_SLOT,
            0, // reference_tick
            shred_version,
            0, // fec_set_index
        );
        shred.copy_to_packet(&mut packet);

        // rejected slot is 2, root is 3
        assert!(should_discard_packet(
            &packet,
            3,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
        assert_eq!(stats.slot_out_of_range, 1);

        assert!(should_discard_packet(
            &packet,
            last_root,
            max_slot,
            345, // shred_version
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
        assert_eq!(stats.shred_version_mismatch, 1);

        // Accepted for 1,3
        assert!(!should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));

        // shreds_received should filter duplicate
        assert!(should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
        assert_eq!(stats.duplicate_shred, 1);

        let shred = Shred::new_from_data(
            1_000_000,
            3,
            0,
            &[],
            ShredFlags::LAST_SHRED_IN_SLOT,
            0,
            0,
            0,
        );
        shred.copy_to_packet(&mut packet);

        // Slot 1 million is too high
        assert!(should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));

        let index = MAX_DATA_SHREDS_PER_SLOT as u32;
        let shred = Shred::new_from_data(5, index, 0, &[], ShredFlags::LAST_SHRED_IN_SLOT, 0, 0, 0);
        shred.copy_to_packet(&mut packet);
        assert!(should_discard_packet(
            &packet,
            last_root,
            max_slot,
            shred_version,
            &hasher,
            &mut shreds_received,
            &mut stats,
        ));
    }
}
