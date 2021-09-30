use anyhow::Context;
use clap::Clap;
use dht::{Dht, Id20, PersistentDht};
use futures::StreamExt;
use librqbit::{
    dht_utils::{read_metainfo_from_peer_receiver, ReadMetainfoResult},
    generate_peer_id,
    peer_connection::PeerConnectionOptions,
    spawn_utils::spawn,
    torrent_from_bytes,
    torrent_manager::TorrentManagerBuilder,
    ByteString, Magnet, TorrentMetaV1Info, TorrentMetaV1Owned,
};
use log::{info, warn};
use reqwest::Url;
use size_format::SizeFormatterBinary as SF;
use std::{net::SocketAddr, str::FromStr, time::Duration};
use tokio::fs::File;
use tokio::io::AsyncReadExt;

async fn torrent_from_url(url: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let response = reqwest::get(url)
        .await
        .with_context(|| format!("error downloading torrent metadata from {}", url))?;
    if !response.status().is_success() {
        anyhow::bail!("GET {} returned {}", url, response.status())
    }
    let b = response
        .bytes()
        .await
        .with_context(|| format!("error reading repsonse body from {}", url))?;
    torrent_from_bytes(&b).context("error decoding torrent")
}

async fn torrent_from_file(filename: &str) -> anyhow::Result<TorrentMetaV1Owned> {
    let mut buf = Vec::new();
    if filename == "-" {
        tokio::io::stdin()
            .read_to_end(&mut buf)
            .await
            .context("error reading stdin")?;
    } else {
        File::open(filename)
            .await
            .with_context(|| format!("error opening {}", filename))?
            .read_to_end(&mut buf)
            .await
            .with_context(|| format!("error reading {}", filename))?;
    }
    torrent_from_bytes(&buf).context("error decoding torrent")
}

#[derive(Debug, Clap)]
enum LogLevel {
    Trace,
    Debug,
    Info,
    Warn,
    Error,
}

#[derive(Debug, Clone, Copy)]
struct ParsedDuration(Duration);
impl FromStr for ParsedDuration {
    type Err = parse_duration::parse::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        parse_duration::parse(s).map(ParsedDuration)
    }
}

#[derive(Clap)]
#[clap(version, author, about)]
struct Opts {
    /// The filename or URL of the torrent. If URL, http/https/magnet are supported.
    torrent_path: String,

    /// The output folder to write to. If not exists, it will be created.
    output_folder: String,

    /// If set, only the file whose filename matching this regex will
    /// be downloaded
    #[clap(short = 'r', long = "filename-re")]
    only_files_matching_regex: Option<String>,

    /// Set if you are ok to write on top of existing files
    #[clap(long)]
    overwrite: bool,

    /// Only list the torrent metadata contents, don't do anything else.
    #[clap(short, long)]
    list: bool,

    /// The loglevel
    #[clap(arg_enum, short = 'v')]
    log_level: Option<LogLevel>,

    /// The interval to poll trackers, e.g. 30s.
    /// Trackers send the refresh interval when we connect to them. Often this is
    /// pretty big, e.g. 30 minutes. This can force a certain value.
    #[clap(short = 'i', long = "tracker-refresh-interval")]
    force_tracker_interval: Option<ParsedDuration>,

    /// The listen address for HTTP API
    #[clap(long = "http-api-listen-addr", default_value = "127.0.0.1:3030")]
    http_api_listen_addr: SocketAddr,

    /// Set this flag if you want to use tokio's single threaded runtime.
    /// It MAY perform better, but the main purpose is easier debugging, as time
    /// profilers work better with this one.
    #[clap(short, long)]
    single_thread_runtime: bool,

    #[clap(long = "disable-dht")]
    disable_dht: bool,

    /// The connect timeout, e.g. 1s, 1.5s, 100ms etc.
    #[clap(long = "peer-connect-timeout")]
    peer_connect_timeout: Option<ParsedDuration>,
}

fn compute_only_files<ByteBuf: AsRef<[u8]>>(
    torrent: &TorrentMetaV1Info<ByteBuf>,
    filename_re: &str,
) -> anyhow::Result<Vec<usize>> {
    let filename_re = regex::Regex::new(&filename_re).context("filename regex is incorrect")?;
    let mut only_files = Vec::new();
    for (idx, (filename, _)) in torrent.iter_filenames_and_lengths()?.enumerate() {
        let full_path = filename
            .to_pathbuf()
            .with_context(|| format!("filename of file {} is not valid utf8", idx))?;
        if filename_re.is_match(full_path.to_str().unwrap()) {
            only_files.push(idx);
        }
    }
    if only_files.is_empty() {
        anyhow::bail!("none of the filenames match the given regex")
    }
    Ok(only_files)
}

fn init_logging(opts: &Opts) {
    if std::env::var_os("RUST_LOG").is_none() {
        match opts.log_level.as_ref() {
            Some(level) => {
                let level_str = match level {
                    LogLevel::Trace => "trace",
                    LogLevel::Debug => "debug",
                    LogLevel::Info => "info",
                    LogLevel::Warn => "warn",
                    LogLevel::Error => "error",
                };
                std::env::set_var("RUST_LOG", level_str);
            }
            None => {
                std::env::set_var("RUST_LOG", "info");
            }
        };
    }
    pretty_env_logger::init();
}

fn main() -> anyhow::Result<()> {
    let opts = Opts::parse();

    init_logging(&opts);

    let mut rt_builder = match opts.single_thread_runtime {
        true => (tokio::runtime::Builder::new_current_thread()),
        false => (tokio::runtime::Builder::new_multi_thread()),
    };

    let rt = rt_builder
        .enable_time()
        .enable_io()
        // the default is 512, it can get out of hand, as this program is CPU-bound on
        // hash checking.
        // note: we aren't using spawn_blocking() anymore, so this doesn't apply,
        // however I'm still messing around, so in case we do, let's block the number of
        // spawned threads.
        .max_blocking_threads(8)
        .build()?;

    rt.block_on(async_main(opts))
}

async fn async_main(opts: Opts) -> anyhow::Result<()> {
    let peer_id = generate_peer_id();
    let dht = if opts.disable_dht {
        None
    } else {
        Some(
            PersistentDht::create(None)
                .await
                .context("error initializing DHT")?,
        )
    };

    let peer_opts = PeerConnectionOptions {
        connect_timeout: opts.peer_connect_timeout.map(|p| p.0),
        ..Default::default()
    };

    // Magnet links are different in that we first need to discover the metadata.
    if opts.torrent_path.starts_with("magnet:") {
        let Magnet {
            info_hash,
            trackers,
        } = Magnet::parse(&opts.torrent_path).context("provided path is not a valid magnet URL")?;

        let dht_rx = dht
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("magnet links without DHT are not supported"))?
            .get_peers(info_hash)
            .await?;

        let trackers = trackers
            .into_iter()
            .filter_map(|url| match reqwest::Url::parse(&url) {
                Ok(url) => Some(url),
                Err(e) => {
                    warn!("error parsing tracker {} as url: {}", url, e);
                    None
                }
            })
            .collect();

        let (info, dht_rx, initial_peers) =
            match read_metainfo_from_peer_receiver(peer_id, info_hash, dht_rx, Some(peer_opts))
                .await
            {
                ReadMetainfoResult::Found { info, rx, seen } => (info, rx, seen),
                ReadMetainfoResult::ChannelClosed { .. } => {
                    anyhow::bail!("DHT died, no way to discover torrent metainfo")
                }
            };
        main_torrent_info(
            opts,
            info_hash,
            info,
            peer_id,
            dht,
            Some(dht_rx),
            initial_peers.into_iter().collect(),
            trackers,
        )
        .await
    } else {
        let torrent = if opts.torrent_path.starts_with("http://")
            || opts.torrent_path.starts_with("https://")
        {
            torrent_from_url(&opts.torrent_path).await?
        } else {
            torrent_from_file(&opts.torrent_path).await?
        };
        let dht_rx = match dht.as_ref() {
            Some(dht) => Some(dht.get_peers(torrent.info_hash).await?),
            None => None,
        };
        let trackers = torrent
            .iter_announce()
            .filter_map(|tracker| {
                let url = match std::str::from_utf8(tracker.as_ref()) {
                    Ok(url) => url,
                    Err(_) => {
                        warn!("cannot parse tracker url as utf-8, ignoring");
                        return None;
                    }
                };
                match Url::parse(url) {
                    Ok(url) => Some(url),
                    Err(e) => {
                        warn!("cannot parse tracker URL {}: {}", url, e);
                        None
                    }
                }
            })
            .collect::<Vec<_>>();
        main_torrent_info(
            opts,
            torrent.info_hash,
            torrent.info,
            peer_id,
            dht,
            dht_rx,
            Vec::new(),
            trackers,
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn main_torrent_info(
    opts: Opts,
    info_hash: Id20,
    info: TorrentMetaV1Info<ByteString>,
    peer_id: Id20,
    dht: Option<Dht>,
    dht_peer_rx: Option<impl StreamExt<Item = SocketAddr> + Unpin + Send + Sync + 'static>,
    initial_peers: Vec<SocketAddr>,
    trackers: Vec<reqwest::Url>,
) -> anyhow::Result<()> {
    info!("Torrent info: {:#?}", &info);
    let only_files = if let Some(filename_re) = opts.only_files_matching_regex {
        let only_files = compute_only_files(&info, &filename_re)?;
        for (idx, (filename, _)) in info.iter_filenames_and_lengths()?.enumerate() {
            if !only_files.contains(&idx) {
                continue;
            }
            info!("Will download {:?}", filename);
        }
        Some(only_files)
    } else {
        None
    };

    if opts.list {
        for (idx, (filename, len)) in info.iter_filenames_and_lengths()?.enumerate() {
            let included = match &only_files {
                Some(files) => files.contains(&idx),
                None => true,
            };
            info!(
                "File {}, size {}{}",
                filename.to_string()?,
                SF::new(len),
                if included { "" } else { "will skip" }
            )
        }
        info!("--list was passed, nothing to do, exiting.");
        return Ok(());
    }

    let http_api_listen_addr = opts.http_api_listen_addr;

    let mut builder = TorrentManagerBuilder::new(info, info_hash, opts.output_folder);
    builder.overwrite(opts.overwrite).peer_id(peer_id);
    if let Some(only_files) = only_files {
        builder.only_files(only_files);
    }
    if let Some(interval) = opts.force_tracker_interval {
        builder.force_tracker_interval(interval.0);
    }
    if let Some(t) = opts.peer_connect_timeout {
        builder.peer_connect_timeout(t.0);
    }

    let http_api = librqbit::http_api::HttpApi::new(dht.clone());
    spawn("HTTP API", {
        let http_api = http_api.clone();
        async move { http_api.make_http_api_and_run(http_api_listen_addr).await }
    });

    let handle = builder.start_manager().await?;
    http_api.add_mgr(handle.clone()).await;

    for url in trackers {
        handle.add_tracker(url).await;
    }
    for peer in initial_peers {
        handle.add_peer(peer).await;
    }

    spawn("Stats printer", {
        let handle = handle.clone();
        async move {
            loop {
                let peer_stats = handle.torrent_state().peer_stats_snapshot().await;
                let stats = handle.torrent_state().stats_snapshot().await;
                let needed = stats.initially_needed_bytes;
                let downloaded_pct = if stats.remaining_bytes == 0 {
                    100f64
                } else {
                    (stats.downloaded_and_checked_bytes as f64 / needed as f64) * 100f64
                };
                info!(
                    "Stats: downloaded {:.2}% ({:.2}), fetched {}, remaining {:.2} out of {:.2}, uploaded {:.2}, have {:.2}, peers: {{live: {}, connecting: {}, queued: {}, seen: {}}}",
                    downloaded_pct,
                    SF::new(stats.downloaded_and_checked_bytes),
                    SF::new(stats.fetched_bytes),
                    SF::new(stats.remaining_bytes),
                    SF::new(needed),
                    SF::new(stats.uploaded_bytes),
                    SF::new(stats.have_bytes),
                    peer_stats.live,
                    peer_stats.connecting,
                    peer_stats.queued,
                    peer_stats.seen,
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    });

    if let Some(mut dht_peer_rx) = dht_peer_rx {
        spawn("DHT peer adder", {
            let handle = handle.clone();
            async move {
                while let Some(peer) = dht_peer_rx.next().await {
                    handle.add_peer(peer).await;
                }
                warn!("dht was closed");
                Ok(())
            }
        });
    }

    handle.wait_until_completed().await?;
    Ok(())
}
