//! The pointer overlay, served to every player page as `/player.js`.
//!
//! The shape is the real Windows pointer, sent by the server as pixels; only its position is
//! ours. That is why it looks native and still moves without waiting for a frame: the pointer is
//! never part of the video.
//!
//! A page only has to say where the picture is drawn:
//!
//! ```js
//! OpenScreenCursor.bind(() => ({ rect: element.getBoundingClientRect(),
//!                                width: element.naturalWidth,
//!                                height: element.naturalHeight }));
//! ```

pub const JS: &str = r##"(() => {
  const pointer = document.getElementById('pointer');

  const style = document.createElement('style');
  style.textContent =
    '#pointer { position: absolute; left: 0; top: 0; pointer-events: none; display: none;' +
    '           background-repeat: no-repeat; background-size: 100% 100%;' +
    '           transform: translate(-200px, -200px); will-change: transform; }';
  document.head.appendChild(style);

  let position = null;
  let shape = null;
  let pendingShape = null;
  let viewport = null;

  function place() {
    const view = viewport ? viewport() : null;
    if (!view || !view.rect || !view.rect.width || !view.width || !position) return;

    const rect = view.rect;
    // Where the picture actually sits: it is letterboxed inside its element.
    const scale = Math.min(rect.width / view.width, rect.height / view.height);
    const left = rect.left + (rect.width - view.width * scale) / 2;
    const top = rect.top + (rect.height - view.height * scale) / 2;

    const hotspotX = shape ? shape.hotspotX * scale : 0;
    const hotspotY = shape ? shape.hotspotY * scale : 0;

    if (shape) {
      pointer.style.width = (shape.width * scale) + 'px';
      pointer.style.height = (shape.height * scale) + 'px';
    }

    pointer.style.transform =
      'translate(' + (left + position[0] * scale - hotspotX) + 'px,' +
      (top + position[1] * scale - hotspotY) + 'px)';
  }

  function applyShape(meta, pixels) {
    const canvas = document.createElement('canvas');
    canvas.width = meta.width;
    canvas.height = meta.height;

    const context = canvas.getContext('2d');
    const image = context.createImageData(meta.width, meta.height);
    image.data.set(new Uint8ClampedArray(pixels));
    context.putImageData(image, 0, 0);

    pointer.style.backgroundImage = 'url(' + canvas.toDataURL() + ')';
    pointer.style.display = 'block';

    shape = meta;
    place();
  }

  function connect() {
    const socket = new WebSocket('ws://' + location.host + '/ws');
    socket.binaryType = 'arraybuffer';
    socket.onclose = () => setTimeout(connect, 1000);

    socket.onmessage = (event) => {
      if (typeof event.data === 'string') {
        let message = null;
        try { message = JSON.parse(event.data); } catch (error) { return; }

        if (message.cursor === null) {
        // The pointer is on a monitor that is not being shared.
        position = null;
        pointer.style.display = 'none';
      } else if (message.cursor) {
        position = message.cursor;
        place();
      }
        if (message.cursorImage) { pendingShape = message.cursorImage; }
        return;
      }

      // The pixels of the shape follow its description.
      if (pendingShape) {
        applyShape(pendingShape, event.data);
        pendingShape = null;
      }
    };
  }

  window.OpenScreenCursor = {
    bind(fn) { viewport = fn; place(); }
  };

  window.addEventListener('resize', place);
  setInterval(place, 250);
  connect();
})();
"##;
