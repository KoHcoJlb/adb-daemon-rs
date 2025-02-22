use eyre::Result;
use eyre::WrapErr;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub async fn read_protocol_string(mut reader: impl AsyncRead + Unpin) -> Result<String> {
    let mut buf = [0; 4];
    reader.read_exact(&mut buf).await?;
    let len = usize::from_str_radix(std::str::from_utf8(&buf)?, 16).context("len")?;

    let mut buf = vec![0; len];
    reader.read_exact(&mut buf).await?;
    let service = String::from_utf8(buf).context("service")?;

    Ok(service)
}

pub async fn write_protocol_string(
    mut writer: impl AsyncWrite + Unpin, data: impl AsRef<[u8]>,
) -> Result<()> {
    writer.write_all(format!("{:04x}", data.as_ref().len()).as_bytes()).await?;
    writer.write_all(data.as_ref()).await?;
    Ok(())
}
