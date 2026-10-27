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
  let socket = null;

  // Where the picture actually sits: it is letterboxed inside its element.
  function picture() {
    const view = viewport ? viewport() : null;
    if (!view || !view.rect || !view.rect.width || !view.width || !view.height) return null;

    const rect = view.rect;
    const scale = Math.min(rect.width / view.width, rect.height / view.height);

    return {
      left: rect.left + (rect.width - view.width * scale) / 2,
      top: rect.top + (rect.height - view.height * scale) / 2,
      width: view.width * scale,
      height: view.height * scale,
      scale: scale
    };
  }

  function place() {
    const box = picture();
    if (!box || !position) return;

    const hotspotX = shape ? shape.hotspotX * box.scale : 0;
    const hotspotY = shape ? shape.hotspotY * box.scale : 0;

    if (shape) {
      pointer.style.width = (shape.width * box.scale) + 'px';
      pointer.style.height = (shape.height * box.scale) + 'px';
    }

    pointer.style.transform =
      'translate(' + (box.left + position[0] * box.scale - hotspotX) + 'px,' +
      (box.top + position[1] * box.scale - hotspotY) + 'px)';
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

  // ---- remote control ----
  //
  // Positions always travel as a fraction of the picture, never as pixels: the same desktop is a
  // different number of pixels on a phone and on a monitor, and the fraction is right on both. The
  // server drops all of it until the panel allows remote control.
  let control = false;
  let pressing = -1;
  let lastMove = 0;
  let wheel = 0;

  function send(text) {
    if (socket && socket.readyState === 1) { socket.send(text); }
  }

  function move(point) {
    send('m ' + point[0] + ' ' + point[1]);
  }

  // Lets go of everything: a Ctrl that nobody is holding any more locks the other keyboard.
  function letGo() {
    if (pressing >= 0) {
      send('b ' + pressing + ' 0');
      pressing = -1;
    }

    send('r');
  }

  // The fraction of the picture under the pointer, or null when it is over the page instead. That
  // is also what keeps clicks on this page's own chrome away from the other machine.
  function locate(event) {
    const box = picture();
    if (!box) return null;

    const x = event.clientX - box.left;
    const y = event.clientY - box.top;

    if (x < 0 || y < 0 || x >= box.width || y >= box.height) return null;

    return [
      Math.min(9999, Math.round(x / box.width * 10000)),
      Math.min(9999, Math.round(y / box.height * 10000))
    ];
  }

  document.addEventListener('pointermove', (event) => {
    if (!control) return;

    const point = locate(event);
    if (!point) return;

    // A touch screen has no hover: once the finger is up the other pointer must stay still.
    if (event.pointerType === 'touch' && pressing < 0) return;

    const now = Date.now();
    if (now - lastMove < 12) return;

    lastMove = now;
    move(point);
  }, { passive: true });

  document.addEventListener('pointerdown', (event) => {
    if (!control || pressing >= 0) return;

    // The "click to start" overlay belongs to this page, not to the other machine.
    if (event.target && event.target.id === 'start') return;

    const point = locate(event);
    if (!point) return;

    event.preventDefault();
    pressing = event.button >= 0 && event.button <= 2 ? event.button : 0;
    move(point);
    send('b ' + pressing + ' 1');
  });

  ['pointerup', 'pointercancel'].forEach((name) => {
    document.addEventListener(name, () => {
      if (pressing < 0) return;

      send('b ' + pressing + ' 0');
      pressing = -1;
    });
  });

  document.addEventListener('contextmenu', (event) => {
    if (control && locate(event)) event.preventDefault();
  });

  document.addEventListener('wheel', (event) => {
    if (!control || !locate(event)) return;

    event.preventDefault();

    // A notch is 120 to the other side, while the browser reports pixels or lines.
    wheel += event.deltaY * (event.deltaMode === 1 ? 33 : event.deltaMode === 2 ? 400 : 1);

    const notches = Math.trunc(wheel / 100);
    if (notches === 0) return;

    wheel -= notches * 100;
    send('w ' + -notches);
  }, { passive: false });

  // Keys this page would otherwise act on. Everything else is left alone, so the browser's own
  // shortcuts still work — they are the browser's, and a page cannot have them back.
  const HANDLED = ['Tab', 'Space', 'Backspace', 'ArrowUp', 'ArrowDown', 'ArrowLeft', 'ArrowRight',
                   'Home', 'End', 'PageUp', 'PageDown', 'Delete'];

  window.addEventListener('keydown', (event) => {
    if (!control) return;

    if (HANDLED.indexOf(event.code) >= 0) event.preventDefault();

    // The physical key: the two computers need not share a keyboard layout. A held key repeats on
    // the other side by itself, because its system sees the key as down.
    if (!event.repeat) send('k ' + event.code + ' 1');
  });

  window.addEventListener('keyup', (event) => {
    if (!control) return;

    if (HANDLED.indexOf(event.code) >= 0) event.preventDefault();

    send('k ' + event.code + ' 0');
  });

  window.addEventListener('blur', letGo);
  document.addEventListener('visibilitychange', () => { if (document.hidden) letGo(); });

  // Whether a click counts is the one thing a viewer cannot guess, so it is on screen: top right,
  // where none of the player pages put anything.
  const badge = document.createElement('div');

  badge.style.cssText =
    'position:fixed;right:10px;top:10px;z-index:2147483647;pointer-events:none;opacity:.8;' +
    'font:13px/1.4 system-ui,-apple-system,"PingFang SC","Microsoft YaHei",sans-serif;' +
    'padding:4px 9px;border-radius:7px;background:rgba(0,0,0,.6);color:#e8e8e8;';

  function paintControl() {
    badge.textContent = control ? '控制已开启' : '只读';
  }

  function mount() {
    if (document.body) {
      document.body.appendChild(badge);
    } else {
      document.addEventListener('DOMContentLoaded', mount);
    }
  }

  mount();
  paintControl();

  function connect() {
    socket = new WebSocket('ws://' + location.host + '/ws');
    socket.binaryType = 'arraybuffer';
    socket.onclose = () => setTimeout(connect, 1000);

    socket.onmessage = (event) => {
      if (typeof event.data === 'string') {
        let message = null;
        try { message = JSON.parse(event.data); } catch (error) { return; }

        if (message.control !== undefined) {
          control = message.control;
          paintControl();
        }

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
