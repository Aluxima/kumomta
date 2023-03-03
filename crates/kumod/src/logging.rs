use crate::mx::ResolvedAddress;
use anyhow::{anyhow, Context};
use async_channel::{Receiver, Sender};
use bounce_classify::{BounceClass, BounceClassifier, BounceClassifierBuilder};
use chrono::{DateTime, Utc};
use message::rfc3464::ReportAction;
use message::Message;
use once_cell::sync::OnceCell;
use rfc5321::{EnhancedStatusCode, Response};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Write;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::Mutex;
use std::thread::JoinHandle;
use zstd::stream::write::{AutoFinishEncoder, Encoder};

static LOGGER: OnceCell<Logger> = OnceCell::new();
static CLASSIFY: OnceCell<BounceClassifier> = OnceCell::new();

#[derive(Deserialize, Clone, Debug)]
pub struct ClassifierParams {
    pub files: Vec<String>,
}

impl ClassifierParams {
    pub fn register(&self) -> anyhow::Result<()> {
        let mut builder = BounceClassifierBuilder::new();
        for file_name in &self.files {
            if file_name.ends_with(".json") {
                builder
                    .merge_json_file(file_name)
                    .map_err(|err| anyhow!("{err}"))?;
            } else if file_name.ends_with(".toml") {
                builder
                    .merge_toml_file(file_name)
                    .map_err(|err| anyhow!("{err}"))?;
            } else {
                anyhow::bail!("{file_name}: classifier files must have either .toml or .json filename extension");
            }
        }

        let classifier = builder.build().map_err(|err| anyhow!("{err}"))?;

        CLASSIFY
            .set(classifier)
            .map_err(|_| anyhow::anyhow!("classifieer already initialized"))?;

        Ok(())
    }
}

#[derive(Deserialize, Clone, Debug)]
pub struct LogFileParams {
    /// Where to place the log files
    pub log_dir: PathBuf,
    /// How many uncompressed bytes to allow per file segment
    #[serde(default = "LogFileParams::default_max_file_size")]
    pub max_file_size: u64,
    /// Maximum number of outstanding items to be logged before
    /// the submission will block; helps to avoid runaway issues
    /// spiralling out of control.
    #[serde(default = "LogFileParams::default_back_pressure")]
    pub back_pressure: usize,

    /// The level of compression.
    /// 0 - use the zstd default level (probably 3).
    /// 1-21 are the explicitly configurable levels
    #[serde(default = "LogFileParams::default_compression_level")]
    pub compression_level: i32,
}

impl LogFileParams {
    fn default_max_file_size() -> u64 {
        1_000_000_000
    }
    fn default_back_pressure() -> usize {
        128_000
    }
    fn default_compression_level() -> i32 {
        0 // use the zstd default
    }
}

enum LogCommand {
    Record(JsonLogRecord),
    Terminate,
}

pub struct Logger {
    sender: Sender<LogCommand>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Logger {
    pub fn get() -> Option<&'static Logger> {
        LOGGER.get()
    }

    pub fn init(params: LogFileParams) -> anyhow::Result<()> {
        std::fs::create_dir_all(&params.log_dir)
            .with_context(|| format!("creating log directory {}", params.log_dir.display()))?;

        let (sender, receiver) = async_channel::bounded(params.back_pressure);
        let thread = std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("create logger runtime");
            runtime.block_on(Self::logger_thread(params, receiver));
        });

        let logger = Self {
            sender,
            thread: Mutex::new(Some(thread)),
        };

        LOGGER
            .set(logger)
            .map_err(|_| anyhow::anyhow!("logger already initialized"))?;
        Ok(())
    }

    async fn logger_thread(params: LogFileParams, receiver: Receiver<LogCommand>) {
        struct OpenedFile {
            file: AutoFinishEncoder<'static, File>,
            name: PathBuf,
            written: u64,
        }

        let mut file: Option<OpenedFile> = None;

        fn do_record(
            params: &LogFileParams,
            file: &mut Option<OpenedFile>,
            mut record: JsonLogRecord,
        ) -> anyhow::Result<()> {
            if let Some(classifier) = CLASSIFY.get() {
                record.bounce_classification = classifier.classify_response(&record.response);
            }
            if file.is_none() {
                let now = Utc::now();
                let name = params.log_dir.join(now.format("%Y%m%d-%H%M%S").to_string());

                let f = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .open(&name)
                    .with_context(|| format!("open log file {name:?}"))?;

                file.replace(OpenedFile {
                    file: Encoder::new(f, params.compression_level)
                        .context("set up zstd encoder")?
                        .auto_finish(),
                    name,
                    written: 0,
                });
            }

            let mut need_rotate = false;

            if let Some(file) = file.as_mut() {
                let mut json = serde_json::to_string(&record).context("serializing record")?;
                json.push_str("\n");
                file.file
                    .write_all(json.as_bytes())
                    .with_context(|| format!("writing record to {}", file.name.display()))?;
                file.written += json.len() as u64;

                need_rotate = file.written >= params.max_file_size;
            }

            if need_rotate {
                file.take();
            }

            Ok(())
        }

        while let Ok(cmd) = receiver.recv().await {
            match cmd {
                LogCommand::Terminate => {
                    break;
                }
                LogCommand::Record(record) => {
                    if let Err(err) = do_record(&params, &mut file, record) {
                        tracing::error!("failed to log: {err:#}");
                    }
                }
            }
        }
    }

    pub async fn log(&self, record: JsonLogRecord) -> anyhow::Result<()> {
        Ok(self.sender.send(LogCommand::Record(record)).await?)
    }

    pub async fn signal_shutdown() {
        if let Some(logger) = Self::get() {
            logger.sender.send(LogCommand::Terminate).await.ok();
            logger
                .thread
                .lock()
                .unwrap()
                .take()
                .map(|thread| thread.join());
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Copy, Clone, Eq, PartialEq)]
pub enum RecordType {
    /// Recorded by a receiving listener
    Reception,
    /// Recorded by the delivery side, most likely as a
    /// result of attempting a delivery to a remote host
    Delivery,
    /// Recorded when a message is expiring from the queue
    Expiration,
    /// Administratively failed
    AdminBounce,
    /// Contains information about an OOB bounce
    OOB,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct JsonLogRecord {
    /// What kind of record this is
    #[serde(rename = "type")]
    pub kind: RecordType,
    /// The message id
    pub id: String,
    /// The envelope sender
    pub sender: String,
    /// The envelope recipient
    pub recipient: String,
    /// Which named queue the message was associated with
    pub queue: String,
    /// Which MX site the message was being delivered to
    pub site: String,
    /// The size of the message, in bytes
    pub size: u64,
    /// The response from/to the peer
    pub response: Response,
    /// The address of the peer, and our sense of its
    /// hostname or EHLO domain
    pub peer_address: Option<ResolvedAddress>,
    /// The time at which we are logging this event
    #[serde(with = "chrono::serde::ts_seconds")]
    pub timestamp: DateTime<Utc>,
    /// The time at which the message was initially received and created
    #[serde(with = "chrono::serde::ts_seconds")]
    pub created: DateTime<Utc>,
    /// The number of delivery attempts that have been made.
    /// Note that this may be approximate after a restart; use the
    /// number of logged events to determine the true number
    pub num_attempts: u16,

    pub bounce_classification: BounceClass,

    pub egress_pool: Option<String>,
    pub egress_source: Option<String>,
}

pub async fn log_disposition(
    kind: RecordType,
    msg: Message,
    site: &str,
    peer_address: Option<&ResolvedAddress>,
    response: Response,
    egress_pool: Option<&str>,
    egress_source: Option<&str>,
) {
    if let Some(logger) = Logger::get() {
        let record = JsonLogRecord {
            kind,
            id: msg.id().to_string(),
            size: msg.get_data().len() as u64,
            sender: msg
                .sender()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|err| format!("{err:#}")),
            recipient: msg
                .recipient()
                .map(|addr| addr.to_string())
                .unwrap_or_else(|err| format!("{err:#}")),
            queue: msg
                .get_queue_name()
                .unwrap_or_else(|err| format!("{err:#}")),
            site: site.to_string(),
            peer_address: peer_address.cloned(),
            response,
            timestamp: Utc::now(),
            created: msg.id().created(),
            num_attempts: msg.get_num_attempts(),
            egress_pool: egress_pool.map(|s| s.to_string()),
            egress_source: egress_source.map(|s| s.to_string()),
            bounce_classification: BounceClass::Uncategorized,
        };
        if let Err(err) = logger.log(record).await {
            tracing::error!("failed to log: {err:#}");
        }

        if kind == RecordType::Reception {
            if let Ok(Some(report)) = msg.parse_rfc3464() {
                // This incoming bounce report is addressed to
                // the envelope from of the original message
                let sender = msg
                    .recipient()
                    .map(|addr| addr.to_string())
                    .unwrap_or_else(|err| format!("{err:#}"));
                let queue = msg
                    .get_queue_name()
                    .unwrap_or_else(|err| format!("{err:#}"));

                for recip in &report.per_recipient {
                    if recip.action != ReportAction::Failed {
                        continue;
                    }

                    let enhanced_code = EnhancedStatusCode {
                        class: recip.status.class,
                        subject: recip.status.subject,
                        detail: recip.status.detail,
                    };

                    let (code, content) = match &recip.diagnostic_code {
                        Some(diag) if diag.diagnostic_type == "smtp" => {
                            if let Some((code, content)) = diag.diagnostic.split_once(' ') {
                                if let Ok(code) = code.parse() {
                                    (code, content.to_string())
                                } else {
                                    (550, diag.diagnostic.to_string())
                                }
                            } else {
                                (550, diag.diagnostic.to_string())
                            }
                        }
                        _ => (550, "".to_string()),
                    };

                    let record = JsonLogRecord {
                        kind: RecordType::OOB,
                        id: msg.id().to_string(),
                        size: 0,
                        sender: sender.clone(),
                        recipient: recip
                            .original_recipient
                            .as_ref()
                            .unwrap_or(&recip.final_recipient)
                            .recipient
                            .to_string(),
                        queue: queue.to_string(),
                        site: site.to_string(),
                        peer_address: Some(ResolvedAddress {
                            name: report.per_message.reporting_mta.name.to_string(),
                            addr: peer_address
                                .map(|a| a.addr)
                                .unwrap_or_else(|| Ipv4Addr::UNSPECIFIED.into()),
                        }),
                        response: Response {
                            code,
                            enhanced_code: Some(enhanced_code),
                            content,
                            command: None,
                        },
                        timestamp: recip.last_attempt_date.unwrap_or_else(|| Utc::now()),
                        created: msg.id().created(),
                        num_attempts: 0,
                        egress_pool: None,
                        egress_source: None,
                        bounce_classification: BounceClass::Uncategorized,
                    };

                    if let Err(err) = logger.log(record).await {
                        tracing::error!("failed to log: {err:#}");
                    }
                }
            }
        }
    }
}
