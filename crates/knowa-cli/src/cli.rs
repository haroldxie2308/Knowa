//! Argument grammar (DESIGN §7), clap derive. Parsing only: every value that needs
//! knowledge of the data model stays a string here and is converted by `run`, so
//! model errors keep their codes (e.g. a bad value is `invalid`, exit 4, not a clap
//! usage error).

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};
use knowa_model::query::DEFAULT_LIMIT;
use knowa_model::{ActorKind, Status, Visibility, vocab};

/// Radius used by `near` and `search --near` when `--km` is not given. Mirrors the
/// `context --km` default that DESIGN §7 documents, so all spatial verbs agree.
pub const DEFAULT_NEAR_KM: &str = "1";

#[derive(Debug, Parser)]
#[command(
    name = "knowa",
    version,
    about = "Knowa: a local-first knowledge base of signed, append-only claims by humans and agents.",
    after_help = "With --json, stdout is one JSON document (the contract) and errors are \
                  {\"error\":{\"code\",\"message\",\"detail\"?}} on stderr. Text output is for humans and may change."
)]
pub struct Cli {
    /// Home directory with actor keys and the space registry [default: ~/.knowa]
    #[arg(long, env = "KNOWA_HOME", global = true, value_name = "DIR")]
    pub home: Option<PathBuf>,

    /// Current space: a registered alias or a replica directory
    #[arg(long, env = "KNOWA_SPACE", global = true, value_name = "ALIAS|DIR")]
    pub space: Option<String>,

    /// Acting actor name. Must come before the verb (`log --actor` is the log's author filter)
    #[arg(long, env = "KNOWA_ACTOR", value_name = "NAME")]
    pub actor: Option<String>,

    /// Print the api result as JSON (contract) instead of text
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub cmd: Cmd,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// Create a space in DIR (genesis, owner kind, label, extract worker grant) and register it
    Init(InitArgs),
    /// Manage this home's space registry
    #[command(subcommand)]
    Space(SpaceCmd),
    /// Manage this home's actor keys
    #[command(subcommand)]
    Actor(ActorCmd),
    /// Write JSONL drafts read from stdin (one draft per line; blank lines are skipped)
    Write,
    /// Run a query document given as JSON, or read from stdin when omitted or `-`
    Query(QueryArgs),
    /// Read claims in local arrival order
    Feed(FeedArgs),
    /// Store or read raw blobs
    #[command(subcommand)]
    Blob(BlobCmd),
    /// Ingest files and directories, then run the built-in extractors
    Put(PutArgs),
    /// Write a note entity
    Note(NoteArgs),
    /// Write one claim
    Assert(AssertArgs),
    /// Retract a claim
    Retract(RetractArgs),
    /// Accept or reject a claim
    Review(ReviewArgs),
    /// Give an actor a role in the current space
    Grant(GrantArgs),
    /// Set the visibility of an id
    Share(ShareArgs),
    /// List claims whose status is proposed
    Pending(PendingArgs),
    /// Full-text search over entities (or claims with --claims)
    Search(SearchArgs),
    /// Entities ordered by world time
    Timeline(TimelineArgs),
    /// Entities nearest to a point or to another entity's place
    Near(NearArgs),
    /// Show entities, claims or blobs by id
    Show(ShowArgs),
    /// An entity with its claims, inbound links, graph, temporal and spatial neighbors
    Context(ContextArgs),
    /// Shortest link paths between two ids
    Path(PathArgs),
    /// The raw audit log, regardless of status
    Log(LogArgs),
    /// Run built-in extractors over blobs missing their output
    Extract(ExtractArgs),
    /// Run external workers over the feed
    #[command(subcommand)]
    Worker(WorkerCmd),
    /// Write a bundle of the current space
    Export(ExportArgs),
    /// Import a bundle as a read-only replica
    Import(ImportArgs),
    /// Verify signatures, chains and blob hashes of the current space
    Verify,
    /// Delete and rebuild the current space's index
    Reindex,
    /// Print the machine-readable contract (ops, documents, vocabulary, error codes)
    Schema,
}

/// Which spaces a read covers.
#[derive(Debug, Args)]
pub struct SpacesArg {
    /// Aliases or space ids, comma-separated; `*` = every registered space [default: current space]
    #[arg(long, value_delimiter = ',', value_name = "A,B|*")]
    pub spaces: Vec<String>,
}

#[derive(Debug, Args)]
pub struct InitArgs {
    /// Directory of the new replica (must not exist, or be empty)
    pub dir: PathBuf,
    /// Human name of the space (its `knowa:label`)
    #[arg(long)]
    pub name: String,
    /// Registry alias [default: the directory's name]
    #[arg(long)]
    pub alias: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum SpaceCmd {
    /// List registered spaces (`*` marks the current one)
    Ls,
    /// Register an existing replica directory
    Add {
        dir: PathBuf,
        #[arg(long)]
        alias: Option<String>,
        /// Refuse local writes to this replica
        #[arg(long)]
        readonly: bool,
    },
    /// Unregister a space (its files are left untouched)
    Rm { alias: String },
    /// Make a space current
    Use { alias: String },
}

#[derive(Debug, Subcommand)]
pub enum ActorCmd {
    /// List actors (`*` marks the current one)
    Ls,
    /// Create an actor key
    New {
        name: String,
        #[arg(long, value_enum)]
        kind: KindArg,
    },
    /// Make an actor current
    Use { name: String },
    /// Show an actor (default: the acting actor)
    Show { name: Option<String> },
}

#[derive(Debug, Args)]
pub struct QueryArgs {
    /// Query document (JSON); `-` or nothing reads stdin
    pub doc: Option<String>,
}

#[derive(Debug, Args)]
pub struct FeedArgs {
    /// Continue after this cursor (`<epoch>:<n>`) [default: from the start]
    #[arg(long)]
    pub cursor: Option<String>,
    /// Claims per batch
    #[arg(long, default_value_t = DEFAULT_LIMIT)]
    pub limit: u32,
    /// Keep polling for new claims; with --json, prints one feed result per line
    #[arg(long)]
    pub follow: bool,
}

#[derive(Debug, Subcommand)]
pub enum BlobCmd {
    /// Store a file's bytes
    Put { file: PathBuf },
    /// Write a blob's raw bytes to stdout
    Cat {
        id: String,
        #[command(flatten)]
        spaces: SpacesArg,
    },
}

#[derive(Debug, Args)]
#[command(after_help = "Directories are walked recursively. Hidden files and directories \
                        (names starting with '.') are skipped and not ingested.")]
pub struct PutArgs {
    /// Files or directories (hidden files are skipped)
    #[arg(required = true)]
    pub paths: Vec<PathBuf>,
    /// Do not run the built-in extractors
    #[arg(long)]
    pub no_extract: bool,
}

#[derive(Debug, Args)]
pub struct NoteArgs {
    /// Note text, or `-` to read it from stdin
    pub text: String,
    /// Title (the note's `knowa:label`)
    #[arg(long)]
    pub title: Option<String>,
    /// World time of the note (EDTF)
    #[arg(long)]
    pub when: Option<String>,
    /// Ids the note is about
    #[arg(long, num_args = 1.., value_name = "ID")]
    pub about: Vec<String>,
}

#[derive(Debug, Args)]
#[command(after_help = "VALUE syntax: text:… term:ns:Name ref:<id> int:42 dec:12.50 bool:true|false \
                        when:<EDTF> geo:lat,lon[,r] qty:12.5:kg — a token without one of these prefixes is text.")]
pub struct AssertArgs {
    /// Subject id (`_:name` mints a new entity)
    pub s: String,
    /// Predicate term (`ns:name`)
    pub p: String,
    /// Object value (see VALUE syntax below)
    pub value: String,
    /// Qualifier `p=VALUE` (repeatable)
    #[arg(long = "q", num_args = 1.., value_name = "P=VALUE")]
    pub q: Vec<String>,
    /// World time the claim holds (EDTF)
    #[arg(long)]
    pub valid: Option<String>,
    /// Input ids this claim was derived from
    #[arg(long, num_args = 1.., value_name = "ID")]
    pub src: Vec<String>,
    /// Confidence, a decimal in [0, 1]
    #[arg(long)]
    pub conf: Option<String>,
    /// Rationale
    #[arg(long)]
    pub note: Option<String>,
    /// Claim ids this claim supersedes
    #[arg(long, num_args = 1.., value_name = "CLAIM")]
    pub replaces: Vec<String>,
    /// Provenance `key=value` (repeatable), e.g. model=…
    #[arg(long, num_args = 1.., value_name = "K=V")]
    pub via: Vec<String>,
}

#[derive(Debug, Args)]
pub struct RetractArgs {
    pub claim: String,
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Debug, Args)]
pub struct ReviewArgs {
    pub claim: String,
    #[arg(value_enum)]
    pub decision: Decision,
    #[arg(long)]
    pub note: Option<String>,
}

#[derive(Debug, Args)]
pub struct GrantArgs {
    /// Actor id (`a_…`)
    pub actor: String,
    #[arg(value_enum)]
    pub role: RoleArg,
    /// Also accept the actor's claims without review
    #[arg(long)]
    pub auto_accept: bool,
}

#[derive(Debug, Args)]
pub struct ShareArgs {
    pub id: String,
    #[arg(value_enum)]
    pub level: Level,
}

#[derive(Debug, Args)]
pub struct PendingArgs {
    #[command(flatten)]
    pub spaces: SpacesArg,
    #[arg(long)]
    pub limit: Option<u32>,
}

#[derive(Debug, Args)]
pub struct SearchArgs {
    /// Free text; every token must match
    pub text: String,
    /// Only entities with any of these `knowa:type` terms
    #[arg(long = "type", num_args = 1.., value_name = "TERM")]
    pub types: Vec<String>,
    /// World-time lower bound (EDTF)
    #[arg(long)]
    pub from: Option<String>,
    /// World-time upper bound (EDTF)
    #[arg(long)]
    pub to: Option<String>,
    /// Only entities near this point
    #[arg(long, value_name = "LAT,LON")]
    pub near: Option<String>,
    /// Radius for --near [default: 1]
    #[arg(long, requires = "near")]
    pub km: Option<String>,
    #[command(flatten)]
    pub spaces: SpacesArg,
    /// Claim statuses to read [default: accepted,proposed]
    #[arg(long, value_enum, value_delimiter = ',')]
    pub status: Vec<StatusArg>,
    /// Only claims by these actor kinds
    #[arg(long, value_enum, value_delimiter = ',')]
    pub kinds: Vec<KindArg>,
    #[arg(long)]
    pub limit: Option<u32>,
    /// Continue a previous single-space search
    #[arg(long)]
    pub cursor: Option<String>,
    /// Return matching claims instead of entities
    #[arg(long)]
    pub claims: bool,
}

#[derive(Debug, Args)]
pub struct TimelineArgs {
    /// World-time lower bound (EDTF)
    #[arg(long)]
    pub from: Option<String>,
    /// World-time upper bound (EDTF)
    #[arg(long)]
    pub to: Option<String>,
    #[arg(long = "type", num_args = 1.., value_name = "TERM")]
    pub types: Vec<String>,
    #[command(flatten)]
    pub spaces: SpacesArg,
    #[arg(long)]
    pub limit: Option<u32>,
}

#[derive(Debug, Args)]
pub struct NearArgs {
    /// `lat,lon` or an id with a `knowa:where`
    pub target: String,
    /// Radius in kilometres
    #[arg(long, default_value = DEFAULT_NEAR_KM)]
    pub km: String,
    #[command(flatten)]
    pub spaces: SpacesArg,
    #[arg(long)]
    pub limit: Option<u32>,
}

#[derive(Debug, Args)]
pub struct ShowArgs {
    #[arg(required = true)]
    pub ids: Vec<String>,
    #[command(flatten)]
    pub spaces: SpacesArg,
}

#[derive(Debug, Args)]
pub struct ContextArgs {
    pub id: String,
    /// Temporal window, ISO 8601 duration [api default: P1D]
    #[arg(long)]
    pub window: Option<String>,
    /// Spatial radius in kilometres [api default: 1]
    #[arg(long)]
    pub km: Option<String>,
    /// Graph depth [api default: 1]
    #[arg(long)]
    pub hops: Option<u32>,
    #[command(flatten)]
    pub spaces: SpacesArg,
}

#[derive(Debug, Args)]
pub struct PathArgs {
    pub from: String,
    pub to: String,
    /// [api default: 4]
    #[arg(long)]
    pub max_hops: Option<u32>,
    #[command(flatten)]
    pub spaces: SpacesArg,
}

#[derive(Debug, Args)]
pub struct LogArgs {
    /// Only claims by these actor ids
    #[arg(long = "actor", num_args = 1.., value_name = "ACTOR_ID")]
    pub actors: Vec<String>,
    /// Record-time lower bound (RFC 3339, inclusive)
    #[arg(long)]
    pub since: Option<String>,
    #[arg(long)]
    pub limit: Option<u32>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[command(flatten)]
    pub spaces: SpacesArg,
}

#[derive(Debug, Args)]
pub struct ExtractArgs {
    /// Re-run every extractor on every blob
    #[arg(long)]
    pub all: bool,
}

#[derive(Debug, Subcommand)]
pub enum WorkerCmd {
    /// Feed batches to an external command and write its JSONL drafts as ACTOR
    Run {
        /// Agent or worker actor name the drafts are written as
        actor: String,
        /// Command and arguments, split with shell-like quoting (no shell is run)
        #[arg(long)]
        cmd: String,
        /// Claims per batch
        #[arg(long, default_value_t = DEFAULT_LIMIT)]
        batch: u32,
        /// Keep polling for new claims
        #[arg(long)]
        follow: bool,
    },
}

#[derive(Debug, Args)]
pub struct ExportArgs {
    /// Bundle file to write
    pub file: PathBuf,
    /// Lowest visibility to include; `private` exports everything
    #[arg(long, value_enum)]
    pub level: Level,
}

#[derive(Debug, Args)]
pub struct ImportArgs {
    /// Bundle file to read
    pub file: PathBuf,
    /// Replica directory [default: <home>/replicas/<space-id>]
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Registry alias [default: the space's label, else its id]
    #[arg(long)]
    pub alias: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum KindArg {
    Human,
    Agent,
    Worker,
}

impl KindArg {
    pub fn kind(self) -> ActorKind {
        match self {
            KindArg::Human => ActorKind::Human,
            KindArg::Agent => ActorKind::Agent,
            KindArg::Worker => ActorKind::Worker,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum RoleArg {
    Owner,
    Member,
    Agent,
    Worker,
    Viewer,
}

impl RoleArg {
    pub fn term(self) -> &'static str {
        match self {
            RoleArg::Owner => vocab::ROLE_OWNER,
            RoleArg::Member => vocab::ROLE_MEMBER,
            RoleArg::Agent => vocab::ROLE_AGENT,
            RoleArg::Worker => vocab::ROLE_WORKER,
            RoleArg::Viewer => vocab::ROLE_VIEWER,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Decision {
    Accept,
    Reject,
}

impl Decision {
    pub fn term(self) -> &'static str {
        match self {
            Decision::Accept => vocab::REVIEW_ACCEPT,
            Decision::Reject => vocab::REVIEW_REJECT,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Level {
    Private,
    Shared,
    Public,
}

impl Level {
    pub fn visibility(self) -> Visibility {
        match self {
            Level::Private => Visibility::Private,
            Level::Shared => Visibility::Shared,
            Level::Public => Visibility::Public,
        }
    }

    pub fn term(self) -> &'static str {
        match self {
            Level::Private => vocab::VIS_PRIVATE,
            Level::Shared => vocab::VIS_SHARED,
            Level::Public => vocab::VIS_PUBLIC,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum StatusArg {
    Accepted,
    Proposed,
    Rejected,
    Retracted,
    Superseded,
}

impl StatusArg {
    pub fn status(self) -> Status {
        match self {
            StatusArg::Accepted => Status::Accepted,
            StatusArg::Proposed => Status::Proposed,
            StatusArg::Rejected => Status::Rejected,
            StatusArg::Retracted => Status::Retracted,
            StatusArg::Superseded => Status::Superseded,
        }
    }
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use super::*;

    #[test]
    fn grammar_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn json_and_space_are_accepted_after_the_verb() {
        let cli = Cli::try_parse_from(["knowa", "search", "fox", "--json", "--space", "main"]).unwrap();
        assert!(cli.json);
        assert_eq!(cli.space.as_deref(), Some("main"));
    }

    #[test]
    fn log_actor_is_the_author_filter_not_the_acting_actor() {
        let cli = Cli::try_parse_from(["knowa", "--actor", "alice", "log", "--actor", "a_x", "a_y"]).unwrap();
        assert_eq!(cli.actor.as_deref(), Some("alice"));
        let Cmd::Log(log) = cli.cmd else { panic!("expected log") };
        assert_eq!(log.actors, ["a_x", "a_y"]);
    }

    #[test]
    fn spaces_status_and_kinds_split_on_commas() {
        let cli = Cli::try_parse_from([
            "knowa", "search", "fox", "--spaces", "a,b", "--status", "accepted,proposed", "--kinds", "human,agent",
        ])
        .unwrap();
        let Cmd::Search(s) = cli.cmd else { panic!("expected search") };
        assert_eq!(s.spaces.spaces, ["a", "b"]);
        assert_eq!(s.status, [StatusArg::Accepted, StatusArg::Proposed]);
        assert_eq!(s.kinds, [KindArg::Human, KindArg::Agent]);
    }

    #[test]
    fn km_without_near_is_a_usage_error() {
        let e = Cli::try_parse_from(["knowa", "search", "fox", "--km", "2"]).unwrap_err();
        assert_eq!(e.exit_code(), 2);
    }

    #[test]
    fn export_requires_a_level() {
        assert!(Cli::try_parse_from(["knowa", "export", "out.knowa"]).is_err());
        let cli = Cli::try_parse_from(["knowa", "export", "out.knowa", "--level", "shared"]).unwrap();
        let Cmd::Export(e) = cli.cmd else { panic!("expected export") };
        assert_eq!(e.level, Level::Shared);
    }

    #[test]
    fn assert_collects_repeated_qualifiers() {
        let cli = Cli::try_parse_from([
            "knowa", "assert", "e_x", "ex:p", "int:3", "--q", "ex:a=int:1", "--q", "ex:b=text:hi", "--via", "model=m1",
        ])
        .unwrap();
        let Cmd::Assert(a) = cli.cmd else { panic!("expected assert") };
        assert_eq!(a.q, ["ex:a=int:1", "ex:b=text:hi"]);
        assert_eq!(a.via, ["model=m1"]);
    }
}
