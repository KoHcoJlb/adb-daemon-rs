use crate::config::config;
use camino::Utf8PathBuf;
use dashmap::DashMap;
use eyre::{Result, WrapErr};
use std::cell::Cell;
use std::fmt::Debug;
use std::fs::{File, create_dir_all};
use std::io::{BufWriter, Seek, Write, stderr};
use std::time::Instant;
use std::{fs, io};
use time::macros::format_description;
use time::{Duration, OffsetDateTime};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Record};
use tracing::{Event, Id, Metadata, Subscriber, error};
use tracing_appender::non_blocking::{NonBlocking, NonBlockingBuilder, WorkerGuard};
use tracing_subscriber::field::RecordFields;
use tracing_subscriber::fmt::format::{DefaultFields, Writer};
use tracing_subscriber::fmt::{FormatFields, MakeWriter};
use tracing_subscriber::layer::{Context, Filter, SubscriberExt};
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, fmt, registry};

// Needed when using multiple fmt::layer()s, otherwise spans' fields will be formatted multiple times
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
    error_timeout: Instant,
    file: Option<File>,
}

impl RollingWriter {
    fn new(dir: Utf8PathBuf, cutoff: u64, count: u32) -> Self {
        Self {
            dir,
            cutoff,
            count,
            error_timeout: Instant::now() - Duration::minutes(2),
            file: Default::default(),
        }
    }

    fn roll(&mut self) -> Result<&mut File> {
        if let Some(file) = &mut self.file
            && file.stream_position().context("stream position")? > self.cutoff
        {
            self.file = None;
        };

        if self.file.is_none() {
            if !self.dir.exists() {
                create_dir_all(&self.dir).context("create log dir")?;
            }

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
                if self.error_timeout.elapsed() > Duration::minutes(1) {
                    self.error_timeout = Instant::now();
                    error!(?err, "roll log file failed");
                }
                Err(io::Error::other(err))
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

#[derive(Default)]
struct DeviceSerial(Option<String>);

impl Visit for DeviceSerial {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "serial" {
            self.0 = Some(value.to_owned());
        }
    }

    fn record_debug(&mut self, _field: &Field, _value: &dyn Debug) {}
}

struct DeviceSerialFilter;

impl DeviceSerialFilter {
    thread_local!(static SERIAL: Cell<Option<String>> = const { Cell::new(None) });
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Filter<S> for DeviceSerialFilter {
    fn enabled(&self, _meta: &Metadata<'_>, _cx: &Context<'_, S>) -> bool {
        true
    }

    fn event_enabled(&self, event: &Event<'_>, ctx: &Context<'_, S>) -> bool {
        let mut serial = DeviceSerial::default();
        event.record(&mut serial);

        let serial = serial.0.or_else(|| {
            ctx.event_scope(event)
                .into_iter()
                .flat_map(|s| s.from_root())
                .filter_map(|s| s.extensions().get::<DeviceSerial>().and_then(|s| s.0.clone()))
                .next()
        });

        Self::SERIAL.set(serial);
        true
    }

    fn on_new_span(&self, attrs: &Attributes<'_>, id: &Id, ctx: Context<'_, S>) {
        let mut s = DeviceSerial::default();
        attrs.record(&mut s);
        ctx.span(id).expect("Span not found").extensions_mut().insert(s);
    }

    fn on_record(&self, span: &Id, values: &Record<'_>, ctx: Context<'_, S>) {
        values.record(
            ctx.span(span)
                .expect("Span not found")
                .extensions_mut()
                .get_mut::<DeviceSerial>()
                .expect("Visitor not found"),
        );
    }
}

struct PerDeviceWriter {
    dir: Utf8PathBuf,
    writers: DashMap<String, (NonBlocking, WorkerGuard)>,
}

impl<'a> MakeWriter<'a> for PerDeviceWriter {
    type Writer = NonBlocking;

    fn make_writer(&'a self) -> Self::Writer {
        let serial = DeviceSerialFilter::SERIAL.take().unwrap_or_default();

        if let Some(writer) = self.writers.get(&serial) {
            writer.0.clone()
        } else {
            let writer = if serial.is_empty() {
                RollingWriter::new(self.dir.clone(), 10 * 1024 * 1024, 5)
            } else {
                RollingWriter::new(self.dir.join(&serial), 20 * 1024 * 1024, 5)
            };

            let (writer, guard) =
                NonBlockingBuilder::default().lossy(false).finish(BufWriter::new(writer));
            self.writers.insert(serial, (writer.clone(), guard));

            writer
        }
    }
}

pub fn init(transport_log: bool) {
    let transport_log = config().transport_log.as_ref().filter(|_| transport_log).map(|path| {
        fmt::layer()
            .with_writer(PerDeviceWriter { dir: path.into(), writers: DashMap::new() })
            .with_ansi(false)
            .fmt_fields(FormatFieldsWrapper::<1>::default())
            .with_filter(EnvFilter::new(
                "adb_daemon_rs=trace,adb_daemon_rs::log=off,adb_transport=trace",
            ))
            .with_filter(DeviceSerialFilter)
    });

    registry()
        .with(transport_log)
        .with(
            fmt::layer().with_writer(stderr).with_filter(
                EnvFilter::try_from_default_env()
                    .unwrap_or(EnvFilter::new("adb_daemon_rs=trace,adb_transport=info")),
            ),
        )
        .init();
}
