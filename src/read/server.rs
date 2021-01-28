use std::path::PathBuf;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::SERVER_WAIT_TIME;

use super::{
    ClientToServerMsg, DataBlock, DataBlockCache, Decoder, FileInfo, HeapData, ServerToClientMsg,
};

pub(crate) struct ReadServer<D: Decoder + 'static> {
    to_client_tx: Producer<ServerToClientMsg<D>>,
    from_client_rx: Consumer<ClientToServerMsg>,
    close_signal_rx: Consumer<Option<HeapData>>,

    decoder: D,
    file_info: FileInfo<D::Params>,

    block_pool: Vec<DataBlock>,
    cache_pool: Vec<DataBlockCache>,

    run: bool,
}

impl<D: Decoder + 'static> ReadServer<D> {
    pub fn new(
        file: PathBuf,
        start_frame_in_file: usize,
        to_client_tx: Producer<ServerToClientMsg<D>>,
        from_client_rx: Consumer<ClientToServerMsg>,
        close_signal_rx: Consumer<Option<HeapData>>,
    ) -> Result<FileInfo<D::Params>, D::OpenError> {
        let (mut open_tx, mut open_rx) =
            RingBuffer::<Result<FileInfo<D::Params>, D::OpenError>>::new(1).split();

        std::thread::spawn(move || {
            match D::new(file, start_frame_in_file) {
                Ok((decoder, file_info)) => {
                    // Push cannot fail because only one message is ever sent.
                    let _ = open_tx.push(Ok(file_info.clone()));

                    ReadServer::run(Self {
                        to_client_tx,
                        from_client_rx,
                        close_signal_rx,
                        decoder,
                        file_info,
                        block_pool: Vec::new(),
                        cache_pool: Vec::new(),
                        run: true,
                    });
                }
                Err(e) => {
                    // Push cannot fail because only one message is ever sent.
                    let _ = open_tx.push(Err(e));
                }
            }
        });

        loop {
            if let Ok(res) = open_rx.pop() {
                return res;
            }

            std::thread::sleep(SERVER_WAIT_TIME);
        }
    }

    fn run(mut self) {
        while self.run {
            // Check for close signal.
            if let Ok(heap_data) = self.close_signal_rx.pop() {
                // Drop heap data here.
                let _ = heap_data;
                self.run = false;
                break;
            }

            while let Ok(msg) = self.from_client_rx.pop() {
                match msg {
                    ClientToServerMsg::ReadIntoBlock {
                        block_index,
                        block,
                        starting_frame_in_file,
                    } => {
                        let mut block = block.unwrap_or(
                            // Try using one in the pool if it exists.
                            self.block_pool.pop().unwrap_or(
                                // No blocks in pool. Create a new one.
                                DataBlock::new(self.file_info.num_channels),
                            ),
                        );

                        block.starting_frame_in_file = starting_frame_in_file;
                        block.wanted_start_smp = starting_frame_in_file;

                        match self.decoder.decode_into(&mut block) {
                            Ok(()) => {
                                self.send_msg(ServerToClientMsg::ReadIntoBlockRes {
                                    block_index,
                                    block,
                                });
                            }
                            Err(e) => {
                                self.send_msg(ServerToClientMsg::FatalError(e));
                                self.run = false;
                                break;
                            }
                        }
                    }
                    ClientToServerMsg::DisposeBlock { block } => {
                        // Store the block to be reused.
                        self.block_pool.push(block);
                    }
                    ClientToServerMsg::SeekTo { frame } => {
                        if let Err(e) = self.decoder.seek_to(frame) {
                            self.send_msg(ServerToClientMsg::FatalError(e));
                            self.run = false;
                            break;
                        }
                    }
                    ClientToServerMsg::Cache {
                        cache_index,
                        cache,
                        starting_frame_in_file,
                    } => {
                        let mut cache = cache.unwrap_or(
                            // Try using one in the pool if it exists.
                            self.cache_pool.pop().unwrap_or(
                                // No caches in pool. Create a new one.
                                DataBlockCache::new(self.file_info.num_channels),
                            ),
                        );

                        cache.wanted_start_smp = starting_frame_in_file;

                        let current_frame = self.decoder.current_frame();

                        // Seek to the position the client wants to cache.
                        if let Err(e) = self.decoder.seek_to(starting_frame_in_file) {
                            self.send_msg(ServerToClientMsg::FatalError(e));
                            self.run = false;
                            break;
                        }

                        // Fill the cache
                        for block in cache.blocks.iter_mut() {
                            if let Err(e) = self.decoder.decode_into(block) {
                                self.send_msg(ServerToClientMsg::FatalError(e));
                                self.run = false;
                                break;
                            }
                        }

                        // Seek back to the previous position.
                        if let Err(e) = self.decoder.seek_to(current_frame) {
                            self.send_msg(ServerToClientMsg::FatalError(e));
                            self.run = false;
                            break;
                        }

                        self.send_msg(ServerToClientMsg::CacheRes { cache_index, cache });
                    }
                    ClientToServerMsg::DisposeCache { cache } => {
                        // Store the cache to be reused.
                        self.cache_pool.push(cache);
                    }
                }
            }

            std::thread::sleep(SERVER_WAIT_TIME);
        }
    }

    fn send_msg(&mut self, msg: ServerToClientMsg<D>) {
        // Block until message can be sent.
        loop {
            if !self.to_client_tx.is_full() {
                break;
            }

            // Check for close signal to avoid waiting forever.
            if let Ok(heap_data) = self.close_signal_rx.pop() {
                // Drop heap data here.
                let _ = heap_data;
                self.run = false;
                break;
            }

            std::thread::sleep(SERVER_WAIT_TIME);
        }

        // Push will never fail because we made sure a slot is available in the
        // previous step (or the server has closed).
        let _ = self.to_client_tx.push(msg);
    }
}
