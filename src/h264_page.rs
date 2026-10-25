//! The player for the H.264 mode.
//!
//! The browser does the decoding, through WebCodecs, which asks the GPU for a hardware decoder.
//! That is what makes 1080p60 cost a couple of milliseconds of CPU on the viewer instead of the
//! eight that a JPEG frame costs, and the stream itself is one to two orders of magnitude
//! smaller than MJPEG.
//!
//! The cursor is still a separate overlay, so it stays crisp and never waits for a frame.

pub const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenScreen (H.264)</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; overflow: hidden; }
  #stage { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; }
  #screen { max-width: 100%; max-height: 100%; display: block; }

  #status { position: fixed; left: 8px; bottom: 6px; color: #8a8a8a;
            font: 12px/1.2 system-ui, sans-serif; user-select: none; }
</style>
</head>
<body>
<div id="stage"><canvas id="screen"></canvas></div>
<div id="pointer"></div>
<div id="status">connecting...</div>
<script src="/player.js"></script>
<script>
(() => {
  const canvas = document.getElementById('screen');
  const context = canvas.getContext('2d', { alpha: false, desynchronized: true });
  const status = document.getElementById('status');

  let decoder = null;
  let configured = false;
  let lastTimestamp = 0;
  let dropped = 0;

  function fail(message) {
    status.textContent = message;
  }

  function configure(config) {
    if (configured) return;

    canvas.width = config.width;
    canvas.height = config.height;

    decoder = new VideoDecoder({
      output(frame) {
        context.drawImage(frame, 0, 0, canvas.width, canvas.height);
        frame.close();
      },
      error(error) { fail('decoder error: ' + error.message); }
    });

    decoder.configure({
      codec: config.codec,
      optimizeForLatency: true,
      hardwareAcceleration: 'prefer-hardware'
    });

    configured = true;
  }

  // Annex-B: a key frame is the one that carries an IDR slice (type 5).
  function isKeyframe(bytes) {
    for (let i = 0; i + 4 < bytes.length; i++) {
      if (bytes[i] === 0 && bytes[i + 1] === 0 && bytes[i + 2] === 1) {
        if ((bytes[i + 3] & 0x1f) === 5) return true;
        i += 2;
      }
    }
    return false;
  }

  function feed(bytes) {
    if (!configured) return;

    // Without a key frame the decoder cannot start, and a growing queue means the viewer is
    // behind: drop everything and wait for the server to send a fresh key frame.
    if (decoder.decodeQueueSize > 30) { dropped++; return; }

    let timestamp = Math.round(performance.now() * 1000);
    if (timestamp <= lastTimestamp) timestamp = lastTimestamp + 1;
    lastTimestamp = timestamp;

    try {
      decoder.decode(new EncodedVideoChunk({
        type: isKeyframe(bytes) ? 'key' : 'delta',
        timestamp: timestamp,
        data: bytes
      }));
    } catch (error) {
      fail('decode failed: ' + error.message);
    }
  }

  function connectVideo() {
    const socket = new WebSocket('ws://' + location.host + '/h264/ws');
    socket.binaryType = 'arraybuffer';

    socket.onopen = () => { status.textContent = 'waiting for video'; };
    socket.onclose = () => { status.textContent = 'reconnecting...'; setTimeout(connectVideo, 500); };

    socket.onmessage = (event) => {
      if (typeof event.data === 'string') {
        try {
          const message = JSON.parse(event.data);
          if (message.config) {
            configure(message.config);
            status.textContent = 'connected (' + message.config.codec + ')';
          }
        } catch (error) { }
        return;
      }

      feed(new Uint8Array(event.data));
    };
  }

  OpenScreenCursor.bind(() => ({
    rect: canvas.getBoundingClientRect(),
    width: canvas.width,
    height: canvas.height
  }));

  if (!('VideoDecoder' in window)) {
    fail('this browser has no WebCodecs (use Chrome or Edge 94+)');
  } else {
    connectVideo();
  }
})();
</script>
</body>
</html>
"##;
