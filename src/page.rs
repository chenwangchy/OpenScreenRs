//! The page for the MJPEG stream.
//!
//! The video is a plain `<img>`, so there is nothing to decode in JavaScript. The pointer comes
//! from `/player.js`, which draws the real Windows pointer as an overlay: it costs no encoding
//! time, stays sharp at any zoom, and never lags behind the picture.

pub const PAGE: &str = r##"<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenScreen</title>
<style>
  html, body { margin: 0; height: 100%; background: #000; overflow: hidden; }
  #stage { position: absolute; inset: 0; display: flex; align-items: center; justify-content: center; }
  #screen { max-width: 100%; max-height: 100%; display: block; }
  #status { position: fixed; left: 8px; bottom: 6px; color: #8a8a8a;
            font: 12px/1.2 system-ui, sans-serif; user-select: none; }
</style>
</head>
<body>
<div id="stage"><img id="screen" src="/stream" alt="shared screen"></div>
<div id="pointer"></div>
<div id="status">connecting...</div>
<script src="/player.js"></script>
<script>
(() => {
  const image = document.getElementById('screen');
  const status = document.getElementById('status');

  function report(text) {
    status.textContent = text;
    document.title = 'OpenScreen - ' + text;
  }

  image.addEventListener('load', () => {
    report('streaming ' + image.naturalWidth + 'x' + image.naturalHeight);
  });

  image.addEventListener('error', () => report('stream failed'));

  OpenScreenCursor.bind(() => ({
    rect: image.getBoundingClientRect(),
    width: image.naturalWidth,
    height: image.naturalHeight
  }));
})();
</script>
</body>
</html>
"##;

/// A tiny reply for anything that is not the stream, so browsers do not hang.
pub const NOT_FOUND: &str =
    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
