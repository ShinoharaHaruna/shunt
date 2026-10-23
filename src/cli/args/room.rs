//! `shunt room` surface (design.md §17).

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct RoomCmd {
    #[command(subcommand)]
    pub action: RoomAction,
}

/// `shunt room` (design.md §17): server-local coordination rooms
/// for the projects sharing compute. `--as <label>` is one identity
/// for author and read cursor — agents use their project name.
#[derive(Subcommand)]
pub enum RoomAction {
    /// Create a room. --members are PROJECT names (the shared
    /// vocabulary); --clients annotates which boxes the room
    /// coordinates and is never a gate
    Create {
        /// Room name (registry-name charset)
        name: String,
        /// Member projects (comma-separated or repeatable)
        #[arg(long, value_delimiter = ',', required = true)]
        members: Vec<String>,
        /// Boxes this room coordinates (annotation only)
        #[arg(long, value_delimiter = ',')]
        clients: Vec<String>,
        #[arg(long)]
        json: bool,
    },
    /// Rooms the --as label belongs to, with unread counts, plus
    /// suggested groups (projects sharing a box with no room yet);
    /// --all lists every room. Never marks.
    Ls {
        /// Identity for unread counts (default `admin`, the
        /// operator identity)
        #[arg(long = "as")]
        as_label: Option<String>,
        /// List every room, not just ones the label belongs to
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Post a message (markdown body, 16 KiB cap) and mark it read
    /// for --as. Body source: --body > --body-file > `-`/stdin
    Say {
        /// Room name or unique prefix
        room: String,
        /// Message body inline
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the body from a file
        #[arg(long)]
        body_file: Option<String>,
        /// `-` reads the body from stdin
        source: Option<String>,
        /// Author + read-cursor label (default `admin`);
        /// case-matched against registered project names, and a
        /// non-member label still posts — recorded member:false
        #[arg(long = "as")]
        as_label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print messages past the caller's mark, or the full stream
    /// with --all; either way marks them read for --as. --digest is
    /// the compacted catch-up view (newest digest + the raw tail
    /// after it); --from/--to give an inclusive audit range across
    /// live + archived messages that never marks.
    Read {
        /// Room name or unique prefix
        room: String,
        /// Reader label (default `admin`)
        #[arg(long = "as")]
        as_label: Option<String>,
        /// Full live stream instead of unread-only
        #[arg(long, conflicts_with_all = ["digest", "from", "to"])]
        all: bool,
        /// Compacted catch-up: newest summary + uncovered tail
        #[arg(long, conflicts_with_all = ["from", "to"])]
        digest: bool,
        /// Range lower bound NNNN (inclusive); reaches the archive
        #[arg(long)]
        from: Option<u32>,
        /// Range upper bound NNNN (inclusive)
        #[arg(long)]
        to: Option<u32>,
        #[arg(long)]
        json: bool,
    },
    /// Conversation compaction: bare form picks the byte-weighted
    /// split and prints the packet an agent feeds its LLM;
    /// `--through N` plus a body source (--body T | --body-file F |
    /// `-`) commits that agent-written digest — shunt archives the
    /// covered originals and never summarizes itself
    Compact {
        /// Room name or unique prefix
        room: String,
        /// Plan phase: covered-bytes fraction, 0 < r < 1 (default 0.8)
        #[arg(long, conflicts_with = "through")]
        ratio: Option<f64>,
        /// Commit phase: the digest covers messages ≤N
        #[arg(long)]
        through: Option<u32>,
        /// Digest body inline
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the digest body from a file
        #[arg(long)]
        body_file: Option<String>,
        /// `-` reads the digest body from stdin
        source: Option<String>,
        /// Author + read-cursor label (default `admin`)
        #[arg(long = "as")]
        as_label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Record the current agreement in meta.decision — latest wins;
    /// the stream retains how it was reached
    Decide {
        /// Room name or unique prefix
        room: String,
        /// The agreement text
        #[arg(long)]
        note: String,
        #[arg(long = "as")]
        as_label: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Print the roster (members, clients, decision, stale marks)
    Members {
        /// Room name or unique prefix
        room: String,
        #[arg(long)]
        json: bool,
    },
    /// Move a room to cold storage (rooms/.archive/<name>/): one
    /// atomic rename preserves meta, msgs, and readers; ops on an
    /// archived room fail naming `unarchive`
    Archive {
        /// Room name or unique prefix (live rooms only)
        room: String,
        #[arg(long)]
        json: bool,
    },
    /// Restore an archived room — exact name only, cold storage
    /// names are typed in full; refuses when a live room already
    /// holds the name
    Unarchive {
        /// Archived room name (exact match only)
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Remove a room and its whole record — the only cleanup; rooms
    /// never auto-expire. Reaches cold storage too: the live name
    /// ladder first, then an exact archived name
    Rm {
        /// Room name or unique prefix (live), or exact archived name
        room: String,
        #[arg(long)]
        json: bool,
    },
}
