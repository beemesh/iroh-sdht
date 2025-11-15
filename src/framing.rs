use anyhow::Result;
use quinn::{RecvStream, SendStream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub async fn write_frame(stream: &mut SendStream, data: &[u8]) -> Result<()> {
    let len = data.len() as u32;
    stream.write_u32_le(len).await?;
    stream.write_all(data).await?;
    Ok(())
}

pub async fn read_frame(stream: &mut RecvStream) -> Result<Option<Vec<u8>>> {
    let len = match stream.read_u32_le().await {
        Ok(v) => v as usize,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
            return Ok(None);
        }
        Err(e) => return Err(e.into()),
    };

    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf).await?;
    Ok(Some(buf))
}
