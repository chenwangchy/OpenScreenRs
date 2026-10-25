//! WebRTC transport: the screen as an H.264 video track over RTP/UDP.
//!
//! Everything else in this program is a blocking, thread per connection design; WebRTC is not, so
//! this module owns a small tokio runtime and talks to the rest of the program through channels.
//! The encoder thread pushes access units in, one peer connection per viewer takes them out.
//!
//! Signalling is deliberately trivial: the SDP goes over the same WebSocket the other modes use,
//! as plain text, and each side gathers its candidates before sending. On a LAN the host
//! candidates are the connection, so there is no STUN, no TURN and no trickle ICE.

use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceConnectionState,
    RTCIceGatheringState, RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;

/// Payload type for the video codec; the same one the offer advertises.
const PAYLOAD_TYPE: u8 = 102;
/// How the track's codec is described. `packetization-mode=1` is what allows a frame to be split
/// over several packets, which any frame bigger than an MTU needs.
fn video_codec(profile_level_id: &str) -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90_000,
            channels: 0,
            sdp_fmtp_line: format!(
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={profile_level_id}"
            ),
            rtcp_feedback: vec![],
        },
        payload_type: PAYLOAD_TYPE,
        ..Default::default()
    }
}

enum Event {
    Gathered,
    Connected,
    Closed,
    /// Reported so a connection that never forms can be explained from the server side.
    Ice(RTCIceConnectionState),
}

struct Handler {
    events: tokio::sync::mpsc::UnboundedSender<Event>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.events.send(Event::Gathered);
        }
    }

    async fn on_ice_connection_state_change(&self, state: RTCIceConnectionState) {
        let _ = self.events.send(Event::Ice(state));
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        match state {
            RTCPeerConnectionState::Connected => {
                let _ = self.events.send(Event::Connected);
            }
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                let _ = self.events.send(Event::Closed);
            }
            _ => {}
        }
    }
}

struct Viewer {
    id: u64,
    units: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    answers: tokio::sync::mpsc::UnboundedSender<String>,
}

/// Owns the runtime and the viewers.
pub struct Hub {
    runtime: tokio::runtime::Handle,
    viewers: Mutex<Vec<Viewer>>,
    next_id: AtomicU64,
    /// What the offer advertises. It follows the resolution, which the panel can change, so it is
    /// updated in place rather than rebuilding the whole transport.
    profile_level_id: Mutex<String>,
    frame_interval: Duration,
}

impl Hub {
    /// Starts the runtime. Returns None when it cannot be created, in which case the caller
    /// should fall back to another mode.
    pub fn new(profile_level_id: String, fps: u32) -> Option<Arc<Self>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .ok()?;

        let handle = runtime.handle().clone();

        std::thread::spawn(move || runtime.block_on(std::future::pending::<()>()));

        Some(Arc::new(Self {
            runtime: handle,
            viewers: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
            profile_level_id: Mutex::new(profile_level_id),
            frame_interval: Duration::from_millis(1000 / fps.max(1) as u64),
        }))
    }

    /// Keeps the advertised `profile-level-id` in step with what the encoder produces.
    pub fn set_profile_level_id(&self, value: String) {
        *self.profile_level_id.lock().unwrap() = value;
    }

    pub fn viewers(&self) -> usize {
        self.viewers.lock().unwrap().len()
    }

    /// Registers a viewer. The offer is written to `writer` once its candidates are gathered.
    pub fn add_viewer(self: &Arc<Self>, writer: TcpStream) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (units_tx, units_rx) = tokio::sync::mpsc::unbounded_channel();
        let (answer_tx, answer_rx) = tokio::sync::mpsc::unbounded_channel();

        self.viewers.lock().unwrap().push(Viewer {
            id,
            units: units_tx,
            answers: answer_tx,
        });

        let codec = video_codec(&self.profile_level_id.lock().unwrap());
        let frame_interval = self.frame_interval;
        let hub = Arc::clone(self);

        self.runtime.spawn(async move {
            if let Err(error) = run_viewer(id, writer, units_rx, answer_rx, codec, frame_interval).await {
                eprintln!("webrtc[{id}]: {error}");
            }

            hub.remove_viewer(id);
        });

        id
    }

    /// Hands the browser's answer to the session it belongs to.
    pub fn accept_answer(&self, id: u64, sdp: String) {
        let viewers = self.viewers.lock().unwrap();

        if let Some(viewer) = viewers.iter().find(|viewer| viewer.id == id) {
            let _ = viewer.answers.send(sdp);
        }
    }

    /// Sends one access unit to every viewer.
    pub fn broadcast(&self, unit: &[u8]) {
        let viewers = self.viewers.lock().unwrap();

        for viewer in viewers.iter() {
            let _ = viewer.units.send(unit.to_vec());
        }
    }

    fn remove_viewer(&self, id: u64) {
        self.viewers.lock().unwrap().retain(|viewer| viewer.id != id);
    }
}

async fn run_viewer(
    id: u64,
    mut writer: TcpStream,
    mut units: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    mut answers: tokio::sync::mpsc::UnboundedReceiver<String>,
    codec: RTCRtpCodecParameters,
    frame_interval: Duration,
) -> Result<(), String> {
    let mut media_engine = MediaEngine::default();
    media_engine
        .register_codec(codec.clone(), RtpCodecKind::Video)
        .map_err(|error| format!("cannot register the codec: {error}"))?;

    let registry = register_default_interceptors(Registry::new(), &mut media_engine)
        .map_err(|error| format!("cannot register interceptors: {error}"))?;

    let (events_tx, mut events) = tokio::sync::mpsc::unbounded_channel();

    let peer = PeerConnectionBuilder::new()
        .with_configuration(
            RTCConfigurationBuilder::default()
                // No STUN, no TURN: the host candidates are the LAN connection.
                .with_ice_servers(vec![RTCIceServer { urls: vec![], ..Default::default() }])
                .build(),
        )
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(Arc::new(Handler { events: events_tx }))
        .with_udp_addrs(vec!["0.0.0.0:0"])
        // UDP is what makes this fast, but a network that drops peer to peer UDP (campus and
        // enterprise WiFi often does) would otherwise leave a viewer staring at nothing. With a
        // passive TCP candidate the browser can fall back to the same transport HTTP uses.
        .with_tcp_addrs(vec!["0.0.0.0:0"])
        .build()
        .await
        .map_err(|error| format!("cannot build the peer connection: {error}"))?;

    // A predictable SSRC keeps the packetizer's bookkeeping simple; it only has to be unique on
    // this connection.
    let ssrc = 0x4f53_5252u32 ^ (id as u32);

    let track = Arc::new(
        TrackLocalStaticSample::new(MediaStreamTrack::new(
            "openscreen".to_owned(),
            "screen".to_owned(),
            "screen".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ))
        .map_err(|error| format!("cannot create the track: {error}"))?,
    );

    let sender = peer
        .add_track(Arc::clone(&track) as Arc<dyn TrackLocal>)
        .await
        .map_err(|error| format!("cannot add the track: {error}"))?;

    let offer = peer
        .create_offer(None)
        .await
        .map_err(|error| format!("cannot create the offer: {error}"))?;

    peer.set_local_description(offer.clone())
        .await
        .map_err(|error| format!("cannot set the local description: {error}"))?;

    // Candidates are gathered after the offer is created and appended to the local description,
    // so wait for gathering and then send what is in the local description: sending the raw
    // offer here would arrive without a single candidate.
    wait_for(&mut events, |event| matches!(event, Event::Gathered), Duration::from_secs(5)).await;

    let local = peer
        .local_description()
        .await
        .map(|description| description.sdp)
        .unwrap_or(offer.sdp);

    if crate::ws::send_text(&mut writer, local.as_bytes()).is_err() {
        return Err("the viewer went away before the offer".to_string());
    }

    let answer = tokio::time::timeout(Duration::from_secs(10), answers.recv())
        .await
        .map_err(|_| "the viewer never sent an answer".to_string())?
        .ok_or_else(|| "the viewer went away".to_string())?;

    let candidates: Vec<String> = answer
        .lines()
        .filter(|line| line.starts_with("a=candidate"))
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            parts.next();
            parts.next()?;
            parts.next()?;
            let transport = parts.next()?;
            let address = parts.next()?;
            let port = parts.next()?;
            Some(format!("{transport} {address}:{port}"))
        })
        .collect();

    let setup = answer
        .lines()
        .find(|line| line.starts_with("a=setup:"))
        .map(|line| line.trim().to_string())
        .unwrap_or_else(|| "a=setup: (missing)".to_string());

    let codecs: Vec<&str> = answer
        .lines()
        .filter(|line| line.starts_with("a=rtpmap:"))
        .map(|line| line.trim())
        .collect();

    eprintln!(
        "webrtc[{id}]: browser offered {} candidate(s): {}",
        candidates.len(),
        candidates.join(", ")
    );
    eprintln!(
        "webrtc[{id}]: answer {} bytes, {setup}, answer codecs: {}",
        answer.len(),
        codecs.join(" | ")
    );

    if codecs.is_empty() {
        // No codec at all means the receiver rejected the media section; the SDP says why.
        let readable = answer.replace("\r\n", " | ");

        eprintln!(
            "webrtc[{id}]: the answer accepted no codec, raw answer: {}",
            &readable[..readable.len().min(900)]
        );
    }

    let description = RTCSessionDescription::answer(answer)
        .map_err(|error| format!("bad answer: {error}"))?;

    peer.set_remote_description(description)
        .await
        .map_err(|error| format!("cannot apply the answer: {error}"))?;

    // Writing samples before the connection is up just wastes packets.
    wait_for(&mut events, |event| matches!(event, Event::Connected), Duration::from_secs(10)).await;

    let payload_type = negotiated_payload_type(&sender)
        .await
        .map_err(|error| format!("no negotiated payload type: {error}"))?;

    eprintln!("webrtc[{id}]: connected");

    loop {
        let unit = tokio::select! {
            unit = units.recv() => match unit {
                Some(unit) => unit,
                None => break,
            },
            event = events.recv() => {
                match event {
                    Some(Event::Ice(state)) => {
                        eprintln!("webrtc[{id}]: ice {state}");
                        continue;
                    }
                    Some(Event::Closed) | None => break,
                    _ => continue,
                }
            }
        };

        // One access unit is one frame; the duration is what the receiver uses to pace it.
        track
            .sample_writer(ssrc, payload_type)
            .write_sample(&Sample {
                data: unit.into(),
                duration: frame_interval,
                ..Default::default()
            })
            .await
            .map_err(|error| format!("cannot write a sample: {error}"))?;
    }

    let _ = peer.close().await;
    eprintln!("webrtc[{id}]: closed");

    Ok(())
}

/// Waits for the first event the predicate accepts, or the timeout.
async fn wait_for<F>(events: &mut tokio::sync::mpsc::UnboundedReceiver<Event>, accept: F, timeout: Duration)
where
    F: Fn(&Event) -> bool,
{
    let _ = tokio::time::timeout(timeout, async {
        while let Some(event) = events.recv().await {
            if accept(&event) {
                return;
            }
        }
    })
    .await;
}

/// The payload type actually negotiated for the sender's single codec. `write_sample` stamps it
/// on every packet and the sender rejects anything it did not negotiate.
async fn negotiated_payload_type(sender: &Arc<dyn RtpSender>) -> Result<u8, webrtc::error::Error> {
    sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|codec| codec.payload_type)
        .ok_or(webrtc::error::Error::ErrSenderWithNoSSRCs)
}
