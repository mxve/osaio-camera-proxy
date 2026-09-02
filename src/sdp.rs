use anyhow::{Context, Result, bail};
use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use std::fmt::Write as _;

pub const VIDEO_PT: u8 = 127;
pub const VIDEO_CLOCK_RATE: u32 = 90000;
pub const AUDIO_PT: u8 = 97;
pub const AUDIO_CLOCK_RATE: u32 = 16000;

const VIDEO_RTX_PT: u8 = 123;
const VIDEO_RTX_RED_PT: u8 = 122;
const VIDEO_RED_PT: u8 = 126;
const VIDEO_FEC_PT: u8 = 125;
const AUDIO_ALT_8K_PT: u8 = 96;
const VIDEO_FB: u8 = 35;
const VIDEO_FBC: u8 = 22;

fn session_key() -> Vec<u8> {
    (0..30).map(|_| rand::random::<u8>() % 255 + 1).collect()
}

fn line<'a>(sdp: &'a str, prefix: &str) -> Option<&'a str> {
    sdp.lines().find(|l| l.starts_with(prefix))
}

fn attr<'a>(sdp: &'a str, prefix: &str) -> Option<&'a str> {
    line(sdp, prefix).map(|l| l[prefix.len()..].trim())
}

fn local_ssrc(block: &str) -> Option<&str> {
    let rest = attr(block, "a=ssrc:")?;
    let end = rest.find(|c: char| !c.is_ascii_digit())?;
    Some(&rest[..end])
}

fn cname_of(block: &str) -> Option<&str> {
    block
        .lines()
        .find_map(|l| l.split_once(" cname:"))
        .map(|(_, cname)| cname.trim())
}

fn m_block<'a>(sdp: &'a str, prefix: &str) -> Option<&'a str> {
    let start = sdp.find(prefix)?;
    let rest = &sdp[start..];
    let end = rest[1..]
        .find("\r\nm=")
        .map(|i| i + 1)
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

pub fn encode_offer(local_sdp: &str) -> Result<(String, u8, u8)> {
    let audio = m_block(local_sdp, "m=audio").context("missing m=audio in offer")?;
    let video = m_block(local_sdp, "m=video").context("missing m=video in offer")?;

    let ufrag = attr(local_sdp, "a=ice-ufrag:").context("missing ice-ufrag")?;
    let pwd = attr(local_sdp, "a=ice-pwd:").context("missing ice-pwd")?;
    let fingerprint = attr(local_sdp, "a=fingerprint:").context("missing fingerprint")?;
    let setup = attr(local_sdp, "a=setup:").context("missing setup")?;
    let origin = attr(local_sdp, "o=").context("missing o= line")?;

    let audio_mid: u8 = attr(audio, "a=mid:")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let video_mid: u8 = attr(video, "a=mid:")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);

    let audio_ssrc = local_ssrc(audio)
        .map(|s| s.to_string())
        .unwrap_or_else(|| rand::random::<u32>().max(1).to_string());
    let audio_cname = cname_of(audio).map(|s| s.to_string()).unwrap_or_else(|| {
        let bytes: [u8; 12] = rand::random();
        base64::engine::general_purpose::STANDARD.encode(bytes)
    });

    let key = session_key();
    let payload = json!({
        "video": {
            "mid": video_mid,
            "sr": 0,
            "dc": 1,
            "pt": VIDEO_PT,
            "fb": VIDEO_FB,
            "fbc": VIDEO_FBC,
            "rm": 1,
            "rr": 1,
            "sc": Vec::<String>::new(),
            "ce": "",
            "md": "",
            "m": 9,
            "pts": VIDEO_CLOCK_RATE,
            "rtx": [VIDEO_RTX_PT, VIDEO_RTX_RED_PT],
            "red": VIDEO_RED_PT,
            "fec": VIDEO_FEC_PT,
            "fmtp": [
                format!("{VIDEO_RTX_PT} apt={VIDEO_PT}"),
                format!("{VIDEO_RTX_RED_PT} apt={VIDEO_RED_PT}"),
            ],
        },
        "audio": {
            "mid": audio_mid,
            "sr": 1,
            "dc": 1,
            "pt": AUDIO_PT,
            "8000": AUDIO_ALT_8K_PT,
            "rm": 1,
            "sc": [audio_ssrc],
            "ce": audio_cname,
            "md": "Lo AID",
            "pts": AUDIO_CLOCK_RATE,
            "m": 9,
        },
        "com": {
            "v": 1,
            "iO": origin,
            "is": 1,
            "iT": "0 0",
            "iG": format!("{audio_mid} {video_mid}"),
            "isc": "WMS Lo",
            "I": "IP4 0.0.0.0",
            "r": "9 IN IP4 0.0.0.0",
            "u": ufrag,
            "p": pwd,
            "o": "trickle renomination",
            "ft": fingerprint,
            "s": setup,
            "ll": 1,
            "abs": 7,
            "tcc": 3,
            "pp": 10,
            "sk": key,
            "rk": key,
        },
    });

    Ok((format!("00\r\n{payload}"), audio_mid, video_mid))
}

#[derive(Deserialize)]
struct Com {
    #[serde(rename = "iO")]
    io: String,
    is: i32,
    #[serde(rename = "iT")]
    it: String,
    #[serde(rename = "iG")]
    ig: String,
    isc: String,
    #[serde(default)]
    ic: String,
    r: String,
    u: String,
    p: String,
    o: String,
    ft: String,
    s: String,
}

#[derive(Deserialize)]
struct MLine {
    sr: i32,
    #[serde(default)]
    dc: i32,
    pt: u32,
    #[serde(default)]
    fbc: u32,
    rm: i32,
    #[serde(default)]
    rr: i32,
    #[serde(default)]
    rs: i32,
    sc: Value,
    ce: String,
    md: String,
    m: u32,
    pts: u32,
}

#[derive(Deserialize)]
struct Answer {
    video: MLine,
    audio: MLine,
    com: Com,
}

fn ssrc_of(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .first()
            .map(|v| {
                v.as_str()
                    .map(String::from)
                    .unwrap_or_else(|| v.to_string())
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

pub fn decode_answer(compact: &str, audio_mid: u8, video_mid: u8) -> Result<String> {
    let Some(json) = compact.strip_prefix("00\r\n") else {
        bail!("invalid compact answer prefix");
    };
    let a: Answer = serde_json::from_str(json).context("parsing compact answer json")?;

    let mut sdp = String::new();
    sdp.push_str("v=0\r\n");
    let _ = write!(sdp, "o={}\r\n", a.com.io);
    if a.com.is == 1 {
        sdp.push_str("s=-\r\n");
    }
    let _ = write!(sdp, "t={}\r\n", a.com.it);
    let _ = write!(sdp, "a=group:{}\r\n", a.com.ig);
    let _ = write!(sdp, "a=msid-semantic:{}\r\n", a.com.isc);

    if audio_mid < video_mid {
        push_media(&mut sdp, "audio", &a.audio, &a.com, audio_mid);
        push_media(&mut sdp, "video", &a.video, &a.com, video_mid);
    } else {
        push_media(&mut sdp, "video", &a.video, &a.com, video_mid);
        push_media(&mut sdp, "audio", &a.audio, &a.com, audio_mid);
    }
    Ok(sdp)
}

fn push_media(sdp: &mut String, kind: &str, m: &MLine, com: &Com, mid: u8) {
    let ssrc = ssrc_of(&m.sc);
    let _ = write!(sdp, "m={kind} {} UDP/TLS/RTP/SAVPF {}\r\n", m.m, m.pt);
    if !com.ic.is_empty() {
        let _ = write!(sdp, "a=candidate:{}\r\n", com.ic);
    }
    let _ = write!(sdp, "a=msid:{}\r\n", m.md);
    if !ssrc.is_empty() {
        let (stream_id, track_id) = m.md.split_once(' ').unwrap_or((m.md.as_str(), ""));
        let _ = write!(sdp, "a=ssrc:{ssrc} cname:{}\r\n", m.ce);
        let _ = write!(sdp, "a=ssrc:{ssrc} msid:{}\r\n", m.md);
        let _ = write!(sdp, "a=ssrc:{ssrc} mslabel:{stream_id}\r\n");
        let _ = write!(sdp, "a=ssrc:{ssrc} label:{track_id}\r\n");
    }
    let _ = write!(sdp, "a=rtcp:{}\r\n", com.r);
    let _ = write!(sdp, "a=ice-ufrag:{}\r\n", com.u);
    let _ = write!(sdp, "a=ice-pwd:{}\r\n", com.p);
    let _ = write!(sdp, "a=ice-options:{}\r\n", com.o);
    let _ = write!(sdp, "a=fingerprint:{}\r\n", com.ft);
    let _ = write!(sdp, "a=setup:{}\r\n", com.s);
    let _ = write!(sdp, "a=mid:{mid}\r\n");

    if kind == "video" {
        if m.sr == 0 {
            sdp.push_str("a=sendonly\r\n");
        }
        if m.rm == 1 {
            sdp.push_str("a=rtcp-mux\r\n");
        }
        if m.rs == 1 {
            sdp.push_str("a=rtcp-rsize\r\n");
        }
        let codec = if m.dc == 1 { "H265" } else { "H264" };
        let _ = write!(sdp, "a=rtpmap:{} {codec}/{}\r\n", m.pt, m.pts);
    } else {
        if m.rm == 1 {
            sdp.push_str("a=rtcp-mux\r\n");
        }
        if m.sr == 1 {
            sdp.push_str("a=sendrecv\r\n");
        }
        if m.rr == 1 || m.rs == 1 {
            sdp.push_str("a=rtcp-rsize\r\n");
        }
        let _ = write!(sdp, "a=rtpmap:{} AAC/{}\r\n", m.pt, m.pts);
    }

    for (bit, name) in [
        (1, "goog-remb"),
        (2, "transport-cc"),
        (4, "ccm fir"),
        (8, "nack pli"),
        (16, "nack"),
    ] {
        if m.fbc & bit != 0 {
            let _ = write!(sdp, "a=rtcp-fb:{} {name}\r\n", m.pt);
        }
    }
}
