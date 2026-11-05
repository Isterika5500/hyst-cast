//! Общий протокол hyst-cast: управляющие сообщения и заголовок медиапакетов.
//!
//! Транспорт — QUIC (quinn):
//! * один двунаправленный поток на соединение — управление: кадры `u32 BE длина + JSON`;
//! * каждый медиапакет (кадр H.264 или пакет Opus) — отдельный однонаправленный поток:
//!   `MediaHeader` (14 байт) + полезная нагрузка. Потерянный/отброшенный кадр не тормозит остальные.

use anyhow::{bail, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const PROTOCOL_VERSION: u32 = 1;
pub const ALPN: &[u8] = b"hyst-cast/1";
/// Максимальный размер управляющего сообщения.
pub const MAX_CTL: usize = 64 * 1024;
/// Максимальный размер одного медиапакета (ключевой кадр 4K с запасом).
pub const MAX_MEDIA: usize = 8 * 1024 * 1024;

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct StreamInfo {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub has_audio: bool,
    /// Чем кодирует стример (для статистики у зрителей).
    pub encoder: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
pub struct PeerInfo {
    pub id: u32,
    pub name: String,
    pub stream: Option<StreamInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t")]
pub enum ClientMsg {
    Hello { version: u32, token: String, name: String, room: String },
    StartStream { info: StreamInfo },
    StopStream,
    Watch { id: u32 },
    Unwatch { id: u32 },
    RequestKeyframe { id: u32 },
    Ping { ts: u64 },
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
#[serde(tag = "t")]
pub enum ServerMsg {
    Welcome { id: u32, peers: Vec<PeerInfo>, max_peers: u32 },
    Error { message: String },
    PeerJoined { peer: PeerInfo },
    PeerLeft { id: u32 },
    StreamStarted { id: u32, info: StreamInfo },
    StreamStopped { id: u32 },
    /// Стримеру: кто-то подключился/отстал — нужен ключевой кадр.
    KeyframeRequest,
    /// Стримеру: кто сейчас смотрит.
    Viewers { names: Vec<String> },
    Pong { ts: u64 },
    /// Стримеру: этому зрителю не хватает канала — стоит снизить битрейт.
    SlowViewer { name: String },
}

pub async fn write_msg<W: AsyncWrite + Unpin, T: Serialize>(w: &mut W, msg: &T) -> Result<()> {
    let data = serde_json::to_vec(msg)?;
    if data.len() > MAX_CTL {
        bail!("управляющее сообщение слишком большое");
    }
    w.write_all(&(data.len() as u32).to_be_bytes()).await?;
    w.write_all(&data).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` — собеседник закрыл поток.
pub async fn read_msg<R: AsyncRead + Unpin, T: DeserializeOwned>(r: &mut R) -> Result<Option<T>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_CTL {
        bail!("управляющее сообщение слишком большое: {len}");
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).await?;
    Ok(Some(serde_json::from_slice(&buf).context("кривое сообщение")?))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaKind {
    Video = 0,
    Audio = 1,
}

pub const FLAG_KEY: u8 = 1;

/// Заголовок медиапакета. `publisher` ставит сервер при пересылке (от стримера приходит 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MediaHeader {
    pub publisher: u32,
    pub kind: MediaKind,
    pub flags: u8,
    /// Время кадра на часах стримера, микросекунды.
    pub pts_us: u64,
}

impl MediaHeader {
    pub const LEN: usize = 14;

    pub fn is_key(&self) -> bool {
        self.flags & FLAG_KEY != 0
    }

    pub fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.publisher.to_be_bytes());
        out.push(self.kind as u8);
        out.push(self.flags);
        out.extend_from_slice(&self.pts_us.to_be_bytes());
    }

    pub fn parse(buf: &[u8]) -> Result<(MediaHeader, &[u8])> {
        if buf.len() < Self::LEN {
            bail!("медиапакет короче заголовка");
        }
        let kind = match buf[4] {
            0 => MediaKind::Video,
            1 => MediaKind::Audio,
            k => bail!("неизвестный тип медиа {k}"),
        };
        Ok((
            MediaHeader {
                publisher: u32::from_be_bytes(buf[0..4].try_into().unwrap()),
                kind,
                flags: buf[5],
                pts_us: u64::from_be_bytes(buf[6..14].try_into().unwrap()),
            },
            &buf[Self::LEN..],
        ))
    }

    /// Переписать поле publisher в уже собранном пакете (на сервере, без копирования полезной нагрузки).
    pub fn set_publisher(packet: &mut [u8], id: u32) {
        packet[0..4].copy_from_slice(&id.to_be_bytes());
    }
}

/// SHA-256 отпечаток сертификата в hex — им клиент «пришпиливает» сервер.
pub fn fingerprint_hex(digest: &[u8]) -> String {
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = MediaHeader { publisher: 7, kind: MediaKind::Audio, flags: FLAG_KEY, pts_us: 123_456_789 };
        let mut v = Vec::new();
        h.write(&mut v);
        v.extend_from_slice(b"payload");
        MediaHeader::set_publisher(&mut v, 42);
        let (p, rest) = MediaHeader::parse(&v).unwrap();
        assert_eq!(p, MediaHeader { publisher: 42, ..h });
        assert!(p.is_key());
        assert_eq!(rest, b"payload");
    }

    #[tokio::test]
    async fn msg_roundtrip() {
        let (mut a, mut b) = tokio::io::duplex(1024);
        let m = ClientMsg::Hello { version: 1, token: "t".into(), name: "Вася".into(), room: "кино".into() };
        write_msg(&mut a, &m).await.unwrap();
        drop(a);
        let got: ClientMsg = read_msg(&mut b).await.unwrap().unwrap();
        assert_eq!(got, m);
        assert!(read_msg::<_, ClientMsg>(&mut b).await.unwrap().is_none());
    }
}
