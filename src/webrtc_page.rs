//! The player for the WebRTC mode.
//!
//! The whole point of this transport is latency: RTP over UDP with no fragment quantisation and
//! no buffering layer in front of the decoder, so a picture can be on screen tens of
//! milliseconds after it was captured. Everything here is therefore geared towards showing the
//! stream as soon as possible and reporting what the connection is actually doing.
//!
//! The cursor is still the overlay from `/player.js`, so it costs nothing and never lags.

pub const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenScreen</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; overflow: hidden; }
  #stage { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; }
  #screen { max-width: 100%; max-height: 100%; display: block; background: #000; }
  #status { position: fixed; left: 8px; top: 8px; color: #e8e8e8; background: rgba(0,0,0,.55);
            padding: 4px 8px; border-radius: 4px; font: 12px/1.3 ui-monospace, monospace;
            user-select: none; pointer-events: none; }
  #start { position: fixed; inset: 0; display: none; align-items: center; justify-content: center;
           color: #eee; background: rgba(0,0,0,.6); font: 16px system-ui, sans-serif; cursor: pointer; }
</style>
</head>
<body>
<div id="stage"><video id="screen" autoplay muted playsinline></video></div>
<div id="pointer"></div>
<div id="status">connecting...</div>
<div id="start">click to start</div>
<script src="/player.js"></script>
<script>
(() => {
  const video = document.getElementById('screen');
  const status = document.getElementById('status');
  const startPrompt = document.getElementById('start');

  let peer = null;
  let socket = null;
  let offerSeen = false;
  let attempts = 0;


  function report(text) {
    status.textContent = text;
    document.title = 'OpenScreen - ' + text;
  }

  function resume() {
    video.muted = true;
    const attempt = video.play();

    if (attempt && attempt.catch) {
      attempt.catch((error) => {
        status.textContent = 'play blocked (' + error.name + '), click to start';
        startPrompt.style.display = 'flex';
      });
    }
  }

  startPrompt.addEventListener('click', () => {
    startPrompt.style.display = 'none';
    resume();
  });

  // What the connection is actually doing. This is the number to look at: a stream that looks
  // rough shows up here as jitter, loss, or a round trip that is not LAN small.
  let lastBytes = 0;
  let lastBytesAt = performance.now();

  async function reportStats() {
    if (!peer) return;

    try {
      const stats = await peer.getStats();
      let decoded = 0;
      let jitter = 0;
      let lost = 0;
      let bytes = 0;
      let selected = null;

      stats.forEach((entry) => {
        if (entry.type === 'inbound-rtp' && entry.kind === 'video') {
          decoded = entry.framesPerSecond || 0;
          jitter = entry.jitter || 0;
          lost = entry.packetsLost || 0;
          bytes = entry.bytesReceived || 0;
        }

        if (entry.type === 'transport' && entry.selectedCandidatePairId) {
          selected = entry.selectedCandidatePairId;
        }
      });

      const now = performance.now();
      const seconds = Math.max(0.001, (now - lastBytesAt) / 1000);
      const mbps = ((bytes - lastBytes) * 8 / 1e6) / seconds;
      lastBytes = bytes;
      lastBytesAt = now;

      const state = peer.connectionState + '/' + peer.iceConnectionState;
      let text = state + ' · ' + decoded.toFixed(0) + ' fps, ' + mbps.toFixed(1) + ' Mbps';

      const pair = selected ? stats.get(selected) : null;
      if (pair && typeof pair.currentRoundTripTime === 'number') {
        text += ', rtt ' + (pair.currentRoundTripTime * 1000).toFixed(0) + 'ms';
      }

      text += ', jitter ' + (jitter * 1000).toFixed(0) + 'ms, lost ' + Math.max(0, lost);
      report(text);
    } catch (error) { }
  }
  async function connect() {
    offerSeen = false;
    peer = new RTCPeerConnection({ iceServers: [] });

    peer.ontrack = (event) => {
      video.srcObject = event.streams[0] || new MediaStream([event.track]);
      resume();
    };

    peer.onconnectionstatechange = () => {
      report('connection ' + peer.connectionState);
    };

    socket = new WebSocket('ws://' + location.host + '/webrtc/ws');

    socket.onmessage = async (event) => {
      if (typeof event.data !== 'string' || offerSeen) return;
      offerSeen = true;

      try {
        // The offer arrives after the server gathered its candidates, so answering once we have
        // ours is all the signalling this needs.
        await peer.setRemoteDescription({ type: 'offer', sdp: event.data });

        const answer = await peer.createAnswer();
        await peer.setLocalDescription(answer);

        if (peer.iceGatheringState !== 'complete') {
          await new Promise((resolve) => {
            const timeout = setTimeout(resolve, 2000);
            peer.addEventListener('icegatheringstatechange', () => {
              if (peer.iceGatheringState === 'complete') {
                clearTimeout(timeout);
                resolve();
              }
            });
          });
        }

        socket.send(peer.localDescription.sdp);
        report('connecting');
      } catch (error) {
        report('signalling failed: ' + error.message);
      }
    };

    // The server creates the transport at startup, but a restart or a dropped connection should
    // not leave the page dead: signalling is one cheap round trip, so retry a few times.
    socket.onclose = () => {
      attempts += 1;

      if (attempts <= 5) {
        report('reconnecting (' + attempts + '/' + 5 + ')');
        setTimeout(connect, 1000 * attempts);
      } else {
        report('signalling closed');
      }
    };

    // If nothing connects, say what that means instead of leaving a silent black page. The
    // common cause is a network that drops peer to peer traffic, and the MediaSource mode uses
    // the same transport as the page itself, so it always works.
    setTimeout(() => {
      if (peer && peer.connectionState !== 'connected') {
        status.textContent = 'not connected: this network may block UDP; try the --mse mode';
      }
    }, 8000);
  }

  OpenScreenCursor.bind(() => ({
    rect: video.getBoundingClientRect(),
    width: video.videoWidth,
    height: video.videoHeight
  }));

  setInterval(reportStats, 1000);

  if (!window.RTCPeerConnection) {
    report('this browser has no WebRTC');
  } else {
    connect();
  }
})();
</script>
</body>
</html>
"##;
