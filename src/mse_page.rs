//! The player for the H.264 mode that works over plain http, on any origin.
//!
//! WebCodecs is a secure context API, so it only works on localhost or https. MediaSource has no
//! such restriction, which is what makes this page usable from another machine over the LAN with
//! nothing but an IP address.
//!
//! Three things about MediaSource matter here:
//!
//! * It only opens once the media element actually starts playing. Chrome's autoplay policy can
//!   leave the element sitting at readyState 0 forever, so `play()` is called as soon as the
//!   source is attached, and a click gets a second chance if that is refused.
//! * It buffers, so playback drifts behind the live edge. The playhead is held close to it by
//!   playing slightly faster, because seeking shows up as a visible jump.
//! * Fragments are cut by time, not at key frames, so a fragment can never be skipped: the only
//!   way out of a backlog is to drop the buffer and wait for the next key frame, which the
//!   server announces.
//!
//! The status line also reports the presented frame rate and the buffer depth, so "the stream is
//! slow" can be told apart from "this machine is behind".

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
  /* top left, clear of the bars phones and tablets put at the bottom */
  #status { position: fixed; left: 8px; top: 8px; color: #e8e8e8; background: rgba(0,0,0,.55);
            padding: 4px 8px; border-radius: 4px; font: 12px/1.3 ui-monospace, monospace;
            user-select: none; pointer-events: none; }
  #start { position: fixed; inset: 0; display: none; align-items: center; justify-content: center;
           color: #eee; background: rgba(0,0,0,.6); font: 16px system-ui, sans-serif; cursor: pointer; }
</style>
</head>
<body>
<div id="stage"><video id="screen" autoplay muted playsinline preload="auto"></video></div>
<div id="pointer"></div>
<div id="status">connecting...</div>
<div id="start">click to start</div>
<script src="/player.js"></script>
<script>
(() => {
  const video = document.getElementById('screen');
  const status = document.getElementById('status');
  const startPrompt = document.getElementById('start');

  let media = null;
  let source = null;
  let pending = [];
  let received = 0;
  let resets = 0;
  let behind = 0;

  // How much buffer to keep. Small means low latency; a stall means it was too small, so the
  // target adapts: it grows when the picture has to wait for data and shrinks again once things
  // have been smooth for a while.
  let target = 0.06;
  let smoothSince = performance.now();
  let fps = 0;
  let waitingForKeyframe = false;
  let nextIsKeyframe = false;

  let presented = 0;
  let presentedAt = performance.now();

  function report() {
    const text = 'live ' + video.currentTime.toFixed(1) + 's, ' + behind.toFixed(2) + 's behind, ' +
      fps.toFixed(0) + ' fps, ' + received + ' fragments' +
      (resets ? ', ' + resets + ' resets' : '');

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

  // Waiting for data is the signal that the buffer target was too small.
  video.addEventListener('waiting', () => {
    target = Math.min(0.2, target + 0.03);
    smoothSince = performance.now();
  });

  startPrompt.addEventListener('click', () => {
    startPrompt.style.display = 'none';
    resume();
  });

  // MediaSource plays whatever it buffered, which is how a stream ends up a second behind.
  // Nudging the rate closes a small gap without the jump a seek makes.
  function catchUp() {
    if (!video.buffered.length) return;

    const end = video.buffered.end(video.buffered.length - 1);
    behind = end - video.currentTime;

    if (behind > target + 0.25) {
      // Hopelessly behind: jump to the live edge.
      video.currentTime = Math.max(0, end - 0.02);
      video.playbackRate = 1;
    } else if (behind > target) {
      video.playbackRate = 1.05;
    } else if (behind < target * 0.4) {
      // Running dry: slow down a hair so a late fragment does not freeze the picture.
      video.playbackRate = 0.97;
    } else {
      video.playbackRate = 1;
    }

    // Been smooth for a while: try a little less buffer.
    const now = performance.now();
    if (now - smoothSince > 8000) {
      target = Math.max(0.04, target - 0.01);
      smoothSince = now;
    }

    if (video.paused) resume();

    // Old fragments are useless once played; dropping them keeps memory flat.
    if (source && !source.updating && video.currentTime > 2) {
      try { source.remove(0, video.currentTime - 1); } catch (error) { }
    }

    report();
  }

  function pump() {
    if (!source || source.updating || !pending.length) return;

    // A time sliced fragment carries no key frame, so nothing may be skipped: decoding needs
    // every one of them. A backlog therefore ends in a reset, not in a dropped fragment.
    if (pending.length > 60) {
      pending = [];
      waitingForKeyframe = true;
      resets++;
      return;
    }

    try {
      source.appendBuffer(pending.shift());
    } catch (error) {
      pending = [];
      waitingForKeyframe = true;
      resets++;
      status.textContent = 'append failed: ' + error.name;
    }
  }

  function start(config) {
    if (media) return;

    media = new MediaSource();

    media.addEventListener('sourceopen', () => {
      try {
        source = media.addSourceBuffer(config.mime);
      } catch (error) {
        status.textContent = 'cannot play ' + config.mime + ' (' + error.name + ')';
        return;
      }

      source.mode = 'segments';
      source.addEventListener('updateend', () => { pump(); catchUp(); });
      resume();
      pump();
    });

    try {
      video.src = URL.createObjectURL(media);
      // Chrome only runs resource selection, and therefore only opens the MediaSource, once the
      // element is told to load or a gesture starts playback.
      video.load();
    } catch (error) {
      status.textContent = 'cannot attach the media source: ' + error.name;
      return;
    }

    resume();

    // Chrome can leave play() pending rather than rejecting it: the element neither loads nor
    // complains. If the source has not opened by now, ask for a click, which always works.
    setTimeout(() => {
      if (media && media.readyState !== 'open') {
        status.textContent = 'click to start';
        startPrompt.style.display = 'flex';
      }
    }, 2000);
  }

  function isInitSegment(bytes) {
    let offset = 0;

    while (offset + 8 <= bytes.length) {
      const size = (bytes[offset] << 24 | bytes[offset + 1] << 16 |
                    bytes[offset + 2] << 8 | bytes[offset + 3]) >>> 0;
      if (size < 8 || offset + size > bytes.length) return false;

      const kind = String.fromCharCode(bytes[offset + 4], bytes[offset + 5],
        bytes[offset + 6], bytes[offset + 7]);
      if (kind === 'moov') return true;
      offset += size;
    }

    return false;
  }

  function connect() {
    const socket = new WebSocket('ws://' + location.host + '/mse/ws');
    socket.binaryType = 'arraybuffer';

    socket.onopen = () => { status.textContent = 'connected, waiting for video'; };
    socket.onclose = () => { status.textContent = 'reconnecting...'; setTimeout(connect, 500); };

    socket.onmessage = (event) => {
      if (typeof event.data === 'string') {
        let message = null;
        try { message = JSON.parse(event.data); } catch (error) { return; }

        if (message.mse) start(message.mse);
        if (message.keyframe) nextIsKeyframe = true;
        return;
      }

      const bytes = new Uint8Array(event.data);
      const keyframe = nextIsKeyframe;
      nextIsKeyframe = false;

      if (isInitSegment(bytes)) {
        pending = [bytes];
        pump();
        return;
      }

      if (waitingForKeyframe) {
        if (!keyframe) return;
        waitingForKeyframe = false;
        pending = [];
      }

      received++;
      pending.push(bytes);
      pump();
    };
  }

  OpenScreenCursor.bind(() => ({
    rect: video.getBoundingClientRect(),
    width: video.videoWidth,
    height: video.videoHeight
  }));

  // How many frames actually reach the screen, which is the number that decides whether the
  // picture looks smooth; it is not always the rate the encoder sends.
  if (video.requestVideoFrameCallback) {
    const count = () => {
      presented++;
      video.requestVideoFrameCallback(count);
    };

    video.requestVideoFrameCallback(count);
  }

  setInterval(() => {
    const now = performance.now();
    fps = (presented * 1000) / Math.max(1, now - presentedAt);
    presented = 0;
    presentedAt = now;
    report();
  }, 1000);

  if (!window.MediaSource) {
    status.textContent = 'this browser has no MediaSource';
  } else {
    connect();
  }
})();
</script>
</body>
</html>
"##;
