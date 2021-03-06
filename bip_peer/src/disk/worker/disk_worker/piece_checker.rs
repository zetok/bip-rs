use std::collections::{HashMap, HashSet};
use std::cmp;

use bip_metainfo::{InfoDictionary, File};
use bip_util::bt::InfoHash;

use disk::error::{TorrentResult, TorrentError, TorrentErrorKind};
use disk::worker::disk_worker::piece_accessor::PieceAccessor;
use disk::fs::{FileSystem};
use message::standard::PieceMessage;

/// Calculates hashes on existing files within the file system given and reports good/bad pieces.
pub struct PieceChecker<'a, F> {
    fs:            F,
    info_dict:     &'a InfoDictionary,
    checker_state: PieceCheckerState
}

impl<'a, F> PieceChecker<'a, F> where F: FileSystem + 'a {
    /// Create a new PieceChecker with an initialized state.
    pub fn new(fs: F, info_dict: &'a InfoDictionary) -> TorrentResult<PieceChecker<'a, F>> {
        let total_blocks = info_dict.pieces().count();
        let last_piece_size = last_piece_size(info_dict);

        let mut piece_checker = PieceChecker::with_state(fs, info_dict, PieceCheckerState::new(total_blocks, last_piece_size));
        
        try!(piece_checker.validate_files_sizes());
        try!(piece_checker.fill_checker_state());
        
        Ok(piece_checker)
    }

    /// Create a new PieceChecker with the given state.
    pub fn with_state(fs: F, info_dict: &'a InfoDictionary, checker_state: PieceCheckerState) -> PieceChecker<'a, F> {
        PieceChecker {
            fs:            fs,
            info_dict:     info_dict,
            checker_state: checker_state
        }
    }

    /// Calculate the diff of old to new good/bad pieces and store them in the piece checker state
    /// to be retrieved by the caller.
    pub fn calculate_diff(mut self) -> TorrentResult<PieceCheckerState> {
        let piece_length = self.info_dict.piece_length() as u64;
        // TODO: Use Block Allocator
        let mut piece_buffer = vec![0u8; piece_length as usize];

        let info_dict = self.info_dict;
        let piece_accessor = PieceAccessor::new(&self.fs, self.info_dict);
        
        try!(self.checker_state.run_with_whole_pieces(piece_length as usize, |message| {
            try!(piece_accessor.read_piece(&mut piece_buffer[..message.block_length()], message));
            
            let calculated_hash = InfoHash::from_bytes(&piece_buffer[..message.block_length()]);
            let expected_hash = InfoHash::from_hash(info_dict
                .pieces()
                .skip(message.piece_index() as usize)
                .next()
                .expect("bip_peer: Piece Checker Failed To Retrieve Expected Hash"))
                .expect("bip_peer: Wrong Length Of Expected Hash Received");
                
            Ok(calculated_hash == expected_hash)
        }));

        Ok(self.checker_state)
    }

    /// Fill the PieceCheckerState with all piece messages for each file in our info dictionary.
    ///
    /// This is done once when a torrent file is added to see if we have any good pieces that
    /// the caller can use to skip (if the torrent was partially downloaded before).
    fn fill_checker_state(&mut self) -> TorrentResult<()> {
        let piece_length = self.info_dict.piece_length() as u64;
        let total_bytes: u64 = self.info_dict.files().map(|file| file.length() as u64).sum();

        let full_pieces = total_bytes / piece_length;
        let last_piece_size = last_piece_size(self.info_dict);

        for piece_index in 0..full_pieces {
            self.checker_state.add_pending_block(PieceMessage::new(piece_index as u32, 0, piece_length as usize));
        }

        if last_piece_size != 0 {
            self.checker_state.add_pending_block(PieceMessage::new(full_pieces as u32, 0, last_piece_size as usize));
        }

        Ok(())
    }

    /// Validates the file sizes for the given torrent file and block allocates them if they do not exist.
    ///
    /// This function will, if the file does not exist, or exists and is zero size, fill the file with zeroes.
    /// Otherwise, if the file exists and it is of the correct size, it will be left alone. If it is of the wrong
    /// size, an error will be thrown as we do not want to overwrite and existing file that maybe just had the same
    /// name as a file in our dictionary.
    fn validate_files_sizes(&mut self) -> TorrentResult<()> {
        for file in self.info_dict.files() {
            let file_path = build_path(self.info_dict.directory(), file);
            let expected_size = file.length() as u64;

            try!(self.fs.open_file(Some(&file_path))
                .map_err(|err| err.into())
                .and_then(|mut file| {
                // File May Or May Not Have Existed Before, If The File Is Zero
                // Length, Assume It Wasn't There (User Doesn't Lose Any Data)
                let actual_size = try!(self.fs.file_size(&file));

                let size_matches = actual_size == expected_size;
                let size_is_zero = actual_size == 0;

                if !size_matches && size_is_zero {
                    self.fs.write_file(&mut file, expected_size - 1, &[0])
                        .expect("bip_peer: Failed To Create File When Validating Sizes");
                } else if !size_matches {
                    return Err(TorrentError::from_kind(TorrentErrorKind::ExistingFileSizeCheck{
                        file_path: file_path,
                        expected_size: expected_size,
                        actual_size: actual_size
                    }))
                }
                
                Ok(())
            }));
        }

        Ok(())
    }
}

fn last_piece_size(info_dict: &InfoDictionary) -> usize {
    let piece_length = info_dict.piece_length() as u64;
    let total_bytes: u64 = info_dict.files().map(|file| file.length() as u64).sum();

    (total_bytes % piece_length) as usize
}

fn build_path(parent_directory: Option<&str>, file: &File) -> String {
    let parent_directory = parent_directory.unwrap_or(".");

    file.paths().fold(parent_directory.to_string(), |mut acc, item| {
        acc.push_str("/");
        acc.push_str(item);

        acc
    })
}

// ----------------------------------------------------------------------------//

/// Stores state for the PieceChecker between invocations.
pub struct PieceCheckerState {
    new_states:      Vec<PieceState>,
    old_states:      HashSet<PieceState>,
    pending_blocks:  HashMap<u32, Vec<PieceMessage>>,
    total_blocks:    usize,
    last_block_size: usize
}

#[derive(PartialEq, Eq, Hash)]
pub enum PieceState {
    /// Piece was discovered as good.
    Good(u32),
    /// Piece was discovered as bad.
    Bad(u32)
}

impl PieceCheckerState {
    /// Create a new PieceCheckerState.
    pub fn new(total_blocks: usize, last_block_size: usize) -> PieceCheckerState {
        PieceCheckerState {
            new_states: Vec::new(),
            old_states: HashSet::new(),
            pending_blocks: HashMap::new(),
            total_blocks: total_blocks,
            last_block_size: last_block_size
        }
    }

    /// Add a pending piece block to the current pending blocks.
    pub fn add_pending_block(&mut self, msg: PieceMessage) {
        self.pending_blocks.entry(msg.piece_index()).or_insert(Vec::new()).push(msg);
    }
    
    /// Run the given closures against NewGood and NewBad messages. Each of the messages will
    /// then either be dropped (NewBad) or converted to OldGood (NewGood).
    pub fn run_with_diff<F>(&mut self, mut callback: F)
        where F: FnMut(&PieceState) {
        for piece_state in self.new_states.drain(..) {
            callback(&piece_state);

            self.old_states.insert(piece_state);
        }
    }

    /// Pass any pieces that have not been identified as OldGood into the callback which determines
    /// if the piece is good or bad so it can be marked as NewGood or NewBad.
    fn run_with_whole_pieces<F>(&mut self, piece_length: usize, mut callback: F) -> TorrentResult<()>
        where F: FnMut(&PieceMessage) -> TorrentResult<bool> {
        self.merge_pieces();

        let mut new_states = &mut self.new_states;
        let old_states = &self.old_states;

        let total_blocks = self.total_blocks;
        let last_block_size = self.last_block_size;

        for messages in self.pending_blocks.values_mut()
            .filter(|ref messages| piece_is_complete(total_blocks, last_block_size, piece_length, messages))
            .filter(|ref messages| !old_states.contains(&PieceState::Good(messages[0].piece_index()))) {
            let is_good = try!(callback(&messages[0]));

            if is_good {
                new_states.push(PieceState::Good(messages[0].piece_index()));
            } else {
                new_states.push(PieceState::Bad(messages[0].piece_index()));
            }

            messages.clear();
        }
        
        Ok(())
    }

    /// Merges all pending piece messages into a single messages if possible.
    fn merge_pieces(&mut self) {
        for (_, ref mut messages) in self.pending_blocks.iter_mut() {
            // Sort the messages by their block offset
            messages.sort_by(|a, b| a.block_offset().cmp(&b.block_offset()));

            let mut messages_len = messages.len();
            let mut merge_success = true;
            // See if we can merge all messages into a single message
            while merge_success && messages_len > 1 {
                let actual_last = messages.pop().expect("bip_peer: Failed To Merge Blocks");
                let second_last = messages.pop().expect("bip_peer: Failed To Merge Blocks");

                let opt_merged =  merge_piece_messages(&second_last, &actual_last);
                if let Some(merged) = opt_merged {
                    messages.push(merged);
                } else {
                    messages.push(second_last);
                    messages.push(actual_last);

                    merge_success = false;
                }

                messages_len = messages.len();
            }
        }
    }
}

/// True if the piece is ready to be hashed and checked (full) as good or not.
fn piece_is_complete(total_blocks: usize, last_block_size: usize, piece_length: usize, messages: &[PieceMessage]) -> bool {
    let is_single_message = messages.len() == 1;
    let is_piece_length = messages.get(0)
        .map(|message| message.block_length() == piece_length)
        .unwrap_or(false);
    let is_last_block = messages.get(0)
        .map(|message| message.piece_index() == (total_blocks - 1) as u32)
        .unwrap_or(false);
    let is_last_block_length = messages.get(0)
        .map(|message| message.block_length() == last_block_size)
        .unwrap_or(false);

    is_single_message && (is_piece_length || (is_last_block && is_last_block_length))
}

/// Merge a piece message a with a piece message b if possible.
///
/// First message's block offset should come before (or at) the block offset of the second message.
fn merge_piece_messages(message_a: &PieceMessage, message_b: &PieceMessage) -> Option<PieceMessage> {
    // Check if the pieces overlap
    let start_a = message_a.block_offset();
    let end_a = start_a + message_a.block_length() as u32;

    let start_b = message_b.block_offset();
    let end_b = start_b + message_b.block_length() as u32;

    // If start b falls between start and end a, then start a is where we start, and we end at the max of end a
    // or end b, then calculate the length from end minus start. Vice versa if a falls between start and end b.
    if start_b >= start_a && start_b <= end_a {
        let end_to_take = cmp::max(end_a, end_b);
        let length = end_to_take - start_a;

        Some(PieceMessage::new(message_a.piece_index(), start_a, length as usize))
    } else if start_a >= start_b && start_a <= end_b {
        let end_to_take = cmp::max(end_a, end_b);
        let length = end_to_take - start_b;

        Some(PieceMessage::new(message_b.piece_index(), start_b, length as usize))
    } else {
        None
    }
}