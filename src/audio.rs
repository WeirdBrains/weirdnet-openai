//! `/v1/audio/speech` and `/v1/audio/transcriptions` over the pool's infer/2 `Speak` and
//! `Transcribe` (weirdnet-agent 0.5.11 and 0.5.8).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use axum::body::{Body, Bytes};
use axum::extract::{FromRequest, Multipart, Request, State};
use axum::http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};
use weirdnet_agent::remote::RemoteError;
use weirdnet_agent::remote::infer::v2::{Meta, Segment, TranscribeRequest, Transcript};
use weirdnet_agent::{AudioFormat, CancelToken};

use crate::{AppState, CancelOnDrop, error, json_body, pool_error, reply_headers, request};

/// `POST /v1/audio/speech`: the request is checked against the pool catalog first (so a voice,
/// format or feature the model lacks is a 400 that names what it has, never a silent remap), then
/// spoken whole or streamed (`stream_format: "audio"`).
pub(crate) async fn speech(State(st): State<Arc<AppState>>, body: Bytes) -> Response {
    let body = match json_body(&body) {
        Ok(v) => v,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e),
    };
    let (mut req, stream) = match request::speech(body) {
        Ok(r) => r,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e.0),
    };
    if let Err(e) = req.check() {
        return pool_error(&st, &e);
    }
    // The catalog names each model's presets, formats and features; with it the refusal lists
    // what the model has. Without a catalog the nodes check the request themselves.
    match st.pool.catalog().await {
        Ok(catalog) => match catalog.route_speak(req.model.as_deref(), &req.needs()) {
            Ok(id) => req.model = Some(id),
            Err(e) => return pool_error(&st, &e),
        },
        Err(e) if e.is_request_error() => return pool_error(&st, &e),
        Err(_) => {}
    }
    let cancel = CancelToken::new();
    let guard = CancelOnDrop(cancel.clone());
    if !stream {
        let spoken = st.pool.speak(req, &cancel).await;
        drop(guard);
        return match spoken {
            Ok(s) => {
                let mut headers = speech_headers(&st, &s.model, s.format, s.sample_rate, s.channels);
                ignored_header(&mut headers, &s.meta);
                (headers, s.audio).into_response()
            }
            Err(e) => pool_error(&st, &e),
        };
    }
    let speech = match st.pool.speak_stream(req, &cancel).await {
        Ok(s) => s,
        Err(e) => return pool_error(&st, &e),
    };
    let h = speech.header();
    let headers = speech_headers(&st, speech.model(), h.format, h.sample_rate, h.channels);
    // After the first audio nothing fails over (another node would start again with another
    // voice draw), so a failure ends the body early instead of splicing two takes.
    let st2 = st.clone();
    let chunks = futures::stream::unfold(Some((speech, guard)), move |state| {
        let st = st2.clone();
        async move {
            let (mut speech, guard) = state?;
            match speech.next().await {
                Ok(Some(data)) => Some((Ok(Bytes::from(data)), Some((speech, guard)))),
                Ok(None) => None,
                Err(e) => {
                    st.metrics.errors.fetch_add(1, Ordering::Relaxed);
                    Some((Err(e), None))
                }
            }
        }
    });
    (headers, Body::from_stream(chunks)).into_response()
}

/// `Content-Type` with the real format (and rate for raw PCM: `audio/pcm;rate=24000`),
/// `x-audio-sample-rate` and `x-weirdnet-audio-channels` when known, and `x-weirdnet-model`.
fn speech_headers(st: &AppState, model: &str, format: AudioFormat, rate: Option<u32>, channels: Option<u16>) -> HeaderMap {
    let mut headers = reply_headers(st, model, false, None);
    let content_type = match (format, rate) {
        (AudioFormat::Pcm16, Some(r)) => format!("{};rate={r}", format.mime()),
        _ => format.mime().to_string(),
    };
    if let Ok(v) = HeaderValue::from_str(&content_type) {
        headers.insert(header::CONTENT_TYPE, v);
    }
    if let Some(r) = rate {
        headers.insert(HeaderName::from_static("x-audio-sample-rate"), HeaderValue::from(r));
    }
    if let Some(c) = channels {
        headers.insert(HeaderName::from_static("x-weirdnet-audio-channels"), HeaderValue::from(c));
    }
    headers
}

/// Request fields the model did not use (such as a reference transcript), as the node named them.
fn ignored_header(headers: &mut HeaderMap, meta: &Meta) {
    if !meta.ignored.is_empty()
        && let Ok(v) = HeaderValue::from_str(&meta.ignored.join(", "))
    {
        headers.insert(HeaderName::from_static("x-weirdnet-ignored"), v);
    }
}

/// How a transcript comes back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TranscriptFormat {
    Json,
    VerboseJson,
    Text,
    Srt,
    Vtt,
}

impl TranscriptFormat {
    fn parse(s: &str) -> Option<Self> {
        Some(match s.trim() {
            "json" => Self::Json,
            "verbose_json" => Self::VerboseJson,
            "text" => Self::Text,
            "srt" => Self::Srt,
            "vtt" => Self::Vtt,
            _ => return None,
        })
    }

    fn timed(self) -> bool {
        matches!(self, Self::VerboseJson | Self::Srt | Self::Vtt)
    }
}

/// The fields of a transcription form.
#[derive(Default)]
struct Form {
    file: Option<(Option<String>, Option<String>, Bytes)>,
    model: Option<String>,
    language: Option<String>,
    prompt: Option<String>,
    response_format: Option<String>,
    granularities: Vec<String>,
    stream: bool,
}

fn non_empty(s: String) -> Option<String> {
    let t = s.trim();
    if t.is_empty() { None } else { Some(t.to_string()) }
}

async fn read_form(req: Request) -> Result<Form, String> {
    let mut mp = Multipart::from_request(req, &()).await.map_err(|e| format!("a transcription request is multipart/form-data: {}", e.body_text()))?;
    let mut form = Form::default();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => return Ok(form),
            Err(e) => return Err(format!("could not read the form: {}", e.body_text())),
        };
        let name = field.name().unwrap_or_default().to_string();
        if name == "file" {
            let file_name = field.file_name().map(str::to_string);
            let content_type = field.content_type().map(str::to_string);
            let bytes = field.bytes().await.map_err(|e| format!("could not read `file`: {}", e.body_text()))?;
            form.file = Some((file_name, content_type, bytes));
            continue;
        }
        let text = field.text().await.map_err(|e| format!("could not read `{name}`: {}", e.body_text()))?;
        match name.as_str() {
            "model" => form.model = non_empty(text),
            "language" => form.language = non_empty(text),
            "prompt" => form.prompt = non_empty(text),
            "response_format" => form.response_format = non_empty(text),
            "timestamp_granularities[]" | "timestamp_granularities" => form.granularities.extend(non_empty(text)),
            "stream" => form.stream = matches!(text.trim(), "true" | "1"),
            // `temperature`, `chunking_strategy`, `include[]`: not used by the pool's transcribers.
            _ => {}
        }
    }
}

/// `POST /v1/audio/transcriptions` (multipart/form-data): `file`, `model`, `language`, `prompt`,
/// `response_format` (`json`, `verbose_json`, `text`, `srt`, `vtt`) and
/// `timestamp_granularities[]=segment`.
pub(crate) async fn transcriptions(State(st): State<Arc<AppState>>, req: Request) -> Response {
    let form = match read_form(req).await {
        Ok(f) => f,
        Err(e) => return error(StatusCode::BAD_REQUEST, "bad_request", &e),
    };
    let Some((file_name, content_type, audio)) = form.file else {
        return error(StatusCode::BAD_REQUEST, "bad_request", "`file` (the audio to transcribe) is required");
    };
    if form.stream {
        return error(StatusCode::BAD_REQUEST, "bad_request", "streamed transcription is not supported; leave `stream` out");
    }
    let format = match form.response_format.as_deref() {
        None => TranscriptFormat::Json,
        Some(f) => match TranscriptFormat::parse(f) {
            Some(f) => f,
            None => {
                return error(
                    StatusCode::BAD_REQUEST,
                    "bad_request",
                    &format!("`response_format` `{f}` is not one of json, verbose_json, text, srt or vtt"),
                );
            }
        },
    };
    if let Some(g) = form.granularities.iter().find(|g| g.as_str() != "segment") {
        return error(StatusCode::BAD_REQUEST, "bad_request", &format!("timestamp granularity `{g}` is not supported (only `segment`)"));
    }
    let Some(audio_format) = audio_format(file_name.as_deref(), content_type.as_deref(), &audio) else {
        return error(
            StatusCode::BAD_REQUEST,
            "bad_request",
            "could not tell the audio's format; send the file with its extension (.wav, .mp3, .flac, .ogg, ...) or its audio/* content type",
        );
    };
    let mut t = TranscribeRequest::new(audio.to_vec(), audio_format).with_timestamps(format.timed() || !form.granularities.is_empty());
    t.model = form.model;
    t.language = form.language;
    t.prompt = form.prompt;
    if let Err(e) = t.check() {
        return pool_error(&st, &e);
    }
    match st.pool.catalog().await {
        Ok(catalog) => match catalog.route_transcribe(t.model.as_deref()) {
            Ok(id) => t.model = Some(id),
            Err(e) => return pool_error(&st, &e),
        },
        Err(e) if e.is_request_error() => return pool_error(&st, &e),
        Err(_) => {}
    }
    let cancel = CancelToken::new();
    let guard = CancelOnDrop(cancel.clone());
    let r = st.pool.transcribe(t, &cancel).await;
    drop(guard);
    let done = match r {
        Ok(d) => d,
        Err(e) => return pool_error(&st, &e),
    };
    let headers = reply_headers(&st, &done.model, done.meta.cache_hit, None);
    let tr = &done.transcript;
    match format {
        TranscriptFormat::Json => (headers, axum::Json(json!({"text": tr.text}))).into_response(),
        TranscriptFormat::VerboseJson => (headers, axum::Json(verbose_json(tr))).into_response(),
        TranscriptFormat::Text => text_reply(headers, "text/plain; charset=utf-8", format!("{}\n", tr.text)),
        TranscriptFormat::Srt | TranscriptFormat::Vtt => {
            let Some(segments) = cues(tr) else {
                return pool_error(
                    &st,
                    &RemoteError::Unsupported {
                        message: format!(
                            "model `{}` returned no timed segments and no duration, so the transcript can't be rendered as subtitles; ask for json or text",
                            done.model
                        ),
                    },
                );
            };
            if format == TranscriptFormat::Srt {
                text_reply(headers, "application/x-subrip; charset=utf-8", srt(&segments))
            } else {
                text_reply(headers, "text/vtt; charset=utf-8", vtt(&segments))
            }
        }
    }
}

fn text_reply(mut headers: HeaderMap, content_type: &'static str, body: String) -> Response {
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    (headers, body).into_response()
}

/// OpenAI's `verbose_json`: `language` (as the node reports it, an ISO 639-1 code) and
/// `duration` in seconds are `null` when unknown.
fn verbose_json(t: &Transcript) -> Value {
    let segments: Vec<Value> = t
        .segments
        .iter()
        .enumerate()
        .map(|(i, s)| json!({"id": i, "start": s.start_ms as f64 / 1000.0, "end": s.end_ms as f64 / 1000.0, "text": s.text}))
        .collect();
    json!({
        "task": "transcribe",
        "language": t.language,
        "duration": t.duration_ms.map(|ms| ms as f64 / 1000.0),
        "text": t.text,
        "segments": segments,
    })
}

/// The cues to render: the segments, or one cue over the whole clip when the backend timed only
/// the clip; `None` when nothing is timed (an empty transcript renders as no cues).
fn cues(t: &Transcript) -> Option<Vec<Segment>> {
    if !t.segments.is_empty() || t.text.trim().is_empty() {
        return Some(t.segments.clone());
    }
    t.duration_ms.map(|ms| vec![Segment::new(0, ms, t.text.clone())])
}

fn timestamp(ms: u64, sep: char) -> String {
    format!("{:02}:{:02}:{:02}{sep}{:03}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000)
}

/// Cue text without blank lines (which end a cue) or `-->` (which starts one).
fn cue_text(s: &str) -> String {
    s.trim().lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("\n").replace("-->", "->")
}

/// SubRip: numbered cues, `HH:MM:SS,mmm`.
fn srt(segments: &[Segment]) -> String {
    segments
        .iter()
        .enumerate()
        .map(|(i, s)| format!("{}\n{} --> {}\n{}\n\n", i + 1, timestamp(s.start_ms, ','), timestamp(s.end_ms, ','), cue_text(&s.text)))
        .collect()
}

/// WebVTT: a header, then cues with `HH:MM:SS.mmm`.
fn vtt(segments: &[Segment]) -> String {
    let mut out = String::from("WEBVTT\n\n");
    for s in segments {
        out.push_str(&format!("{} --> {}\n{}\n\n", timestamp(s.start_ms, '.'), timestamp(s.end_ms, '.'), cue_text(&s.text)));
    }
    out
}

/// The audio's format name for the node (`wav`, `mp3`, ...): from the file name's extension, else
/// the part's `audio/*` content type, else the first bytes.
fn audio_format(file_name: Option<&str>, content_type: Option<&str>, bytes: &[u8]) -> Option<String> {
    if let Some(ext) = file_name.and_then(|n| n.rsplit_once('.')).map(|(_, e)| e.to_ascii_lowercase()) {
        let f = match ext.as_str() {
            "wav" | "wave" => Some("wav"),
            "mp3" | "mpeg" | "mpga" => Some("mp3"),
            "flac" => Some("flac"),
            "ogg" | "oga" | "opus" => Some("ogg"),
            "webm" => Some("webm"),
            "m4a" | "mp4" | "aac" => Some("m4a"),
            _ => None,
        };
        if let Some(f) = f {
            return Some(f.to_string());
        }
    }
    if let Some(ct) = content_type {
        let ct = ct.trim().to_ascii_lowercase();
        if ct.starts_with("audio/") || ct.starts_with("video/webm") || ct.starts_with("video/mp4") {
            return Some(request::audio_format_of_mime(&ct));
        }
    }
    let f = match bytes {
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'A', b'V', b'E', ..] => "wav",
        [b'I', b'D', b'3', ..] => "mp3",
        [0xFF, b, ..] if b & 0xE0 == 0xE0 => "mp3",
        [b'f', b'L', b'a', b'C', ..] => "flac",
        [b'O', b'g', b'g', b'S', ..] => "ogg",
        [0x1A, 0x45, 0xDF, 0xA3, ..] => "webm",
        [_, _, _, _, b'f', b't', b'y', b'p', ..] => "m4a",
        _ => return None,
    };
    Some(f.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtitles_render_segments() {
        let segs = vec![Segment::new(0, 1500, " Hello there. "), Segment::new(3_723_004, 3_725_000, "a --> b\n\nnext")];
        assert_eq!(srt(&segs), "1\n00:00:00,000 --> 00:00:01,500\nHello there.\n\n2\n01:02:03,004 --> 01:02:05,000\na -> b\nnext\n\n");
        assert_eq!(vtt(&segs), "WEBVTT\n\n00:00:00.000 --> 00:00:01.500\nHello there.\n\n01:02:03.004 --> 01:02:05.000\na -> b\nnext\n\n");
        assert_eq!(vtt(&[]), "WEBVTT\n\n");
    }

    #[test]
    fn cues_fall_back_to_the_clip_or_refuse() {
        let t = Transcript::new("hi").with_duration_ms(2000);
        assert_eq!(cues(&t), Some(vec![Segment::new(0, 2000, "hi")]));
        assert_eq!(cues(&Transcript::new("hi")), None);
        assert_eq!(cues(&Transcript::new("")), Some(vec![]));
    }

    #[test]
    fn audio_formats_from_name_type_or_bytes() {
        assert_eq!(audio_format(Some("a.WAV"), None, b""), Some("wav".into()));
        assert_eq!(audio_format(Some("a.mpga"), None, b""), Some("mp3".into()));
        assert_eq!(audio_format(Some("clip"), Some("audio/x-wav"), b""), Some("wav".into()));
        assert_eq!(audio_format(None, Some("audio/mpeg"), b""), Some("mp3".into()));
        assert_eq!(audio_format(None, Some("application/octet-stream"), b"RIFF\0\0\0\0WAVEfmt "), Some("wav".into()));
        assert_eq!(audio_format(None, None, b"fLaC\0"), Some("flac".into()));
        assert_eq!(audio_format(None, None, b"OggS\0"), Some("ogg".into()));
        assert_eq!(audio_format(Some("notes.txt"), Some("text/plain"), b"hello"), None);
    }

    #[test]
    fn transcript_formats() {
        assert_eq!(TranscriptFormat::parse("verbose_json"), Some(TranscriptFormat::VerboseJson));
        assert!(TranscriptFormat::Srt.timed() && !TranscriptFormat::Text.timed());
        assert_eq!(TranscriptFormat::parse("tsv"), None);
    }
}
