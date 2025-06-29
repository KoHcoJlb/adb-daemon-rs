use crate::config::config;
use camino::Utf8PathBuf;
use eyre::{Result, WrapErr};
use std::fs::File;
use std::io::{BufWriter, Seek, Write, stderr};
use std::time::Instant;
use std::{fs, io};
use time::macros::format_description;
use time::{Duration, OffsetDateTime};
use tracing::error;
use tracing_appender::non_blocking::{NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::FormatFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt, registry};

#[derive(Default)]
struct FormatFieldsWrapper<const ID: usize>(DefaultFields);

impl<'wr, const ID: usize> FormatFields<'wr> for FormatFieldsWrapper<ID> {
    fn format_fields<R: RecordFields>(&self, writer: Writer<'wr>, fields: R) -> std::fmt::Result {
        self.0.format_fields(writer, fields)
    }
}

struct RollingWriter {
    dir: Utf8PathBuf,
    cutoff: u64,
    count: u32,
    timeout: Instant,
    file: Option<File>,
}

impl RollingWriter {
    fn new(dir: Utf8PathBuf, cutoff: u64, count: u32) -> Self {
        Self {
            dir,
            cutoff,
            count,
            timeout: Instant::now() - Duration::minutes(2),
            file: Default::default(),
        }
    }

    fn roll(&mut self) -> Result<&mut File> {
        if let Some(writer) = &self.file {
            let mut writer = writer;
            if writer.stream_position().context("stream position")? > self.cutoff {
                self.file = None;
            };
        }

        if self.file.is_none() {
            let mut files = self
                .dir
                .read_dir_utf8()
                .context("read log dir")?
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_owned())
                .filter(|name| name.ends_with(".log"))
                .collect::<Vec<_>>();

            files.sort();

            for file in files.into_iter().rev().skip(self.count as usize).map(|n| self.dir.join(n))
            {
                fs::remove_file(&file).context("remove old log file")?;
            }

            self.file = Some(
                File::create(
                    self.dir
                        .join(
                            OffsetDateTime::now_utc()
                                .format(format_description!(
                                    "[year]_[month]_[day]__[hour]_[minute]_[second]"
                                ))
                                .unwrap(),
                        )
                        .with_extension("log"),
                )
                .context("create log file")?,
            );
        }

        Ok(self.file.as_mut().unwrap())
    }
}

impl Write for RollingWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self.roll() {
            Ok(file) => file.write(buf),
            Err(err) => {
                if self.timeout.elapsed() > Duration::minutes(1) {
                    self.timeout = Instant::now();
                    error!(?err, "roll log file failed");
                }
                Err(io::Error::new(io::ErrorKind::Other, err))
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        if let Some(file) = &mut self.file {
            file.flush()?
        }
        Ok(())
    }
}

pub fn init(transport_log: bool) -> Option<WorkerGuard> {
    let (transport_log, guard) = config().transport_log.as_ref().filter(|_| transport_log)
        .map(|path| {
            let (writer, guard) = NonBlockingBuilder::default()
                .lossy(false)
                .finish(BufWriter::new(RollingWriter::new(path.clone(), 100 * 1024 * 1024, 20)));

            (
                fmt::layer()
                    .with_writer(writer)
                    .with_ansi(false)
                    .fmt_fields(FormatFieldsWrapper::<1>::default())
                    .with_filter(EnvFilter::try_from_default_env().unwrap_or(EnvFilter::new(
                        "adb_daemon_rs=trace,adb_daemon_rs::log=off,adb_transport=trace,adb_transport::connection=trace"
                    ))),
                guard,
            )
        }).unzip();

    registry()
        .with(transport_log)
        .with(
            fmt::layer()
                .with_writer(stderr)
                .with_filter(EnvFilter::new("adb_daemon_rs=trace,adb_transport=info")),
        )
        .init();

    guard
}
