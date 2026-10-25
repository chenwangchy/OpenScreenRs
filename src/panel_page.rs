//! The control panel.
//!
//! The program already serves HTTP on the LAN, so the cheapest honest GUI is a page on that
//! server: nothing new to link, nothing to install, and it can be operated from the same iPad or
//! phone that watches the screen. Changing something here restarts only the encoder; the process,
//! the port and this page stay up.

pub const PAGE: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>OpenScreen 控制面板</title>
<style>
  :root { color-scheme: dark; }
  * { box-sizing: border-box; }
  body { margin: 0; padding: 20px; background: #16181d; color: #e6e6e6;
         font: 15px/1.6 system-ui, -apple-system, "Segoe UI", "Microsoft YaHei", sans-serif; }
  main { max-width: 640px; margin: 0 auto; }
  h1 { font-size: 20px; margin: 0 0 4px; font-weight: 600; }
  p.sub { margin: 0 0 20px; color: #9aa0a6; font-size: 13px; }
  section { background: #1e2127; border: 1px solid #2b2f38; border-radius: 10px;
            padding: 16px; margin-bottom: 14px; }
  h2 { font-size: 12px; letter-spacing: .08em; color: #8b93a1; margin: 0 0 12px;
       font-weight: 600; }
  .modes { display: grid; grid-template-columns: 1fr 1fr; gap: 8px; }
  button.mode { text-align: left; padding: 10px 12px; border-radius: 8px; cursor: pointer;
                background: #23262e; color: #d7dae0; border: 1px solid #333844;
                font: inherit; }
  button.mode b { display: block; font-size: 14px; }
  button.mode span { display: block; font-size: 12px; color: #8b93a1; margin-top: 2px; }
  button.mode.on { background: #2b4a7a; border-color: #4b7fd0; color: #fff; }
  button.mode.on span { color: #c3d8f7; }
  label.row { display: flex; align-items: center; justify-content: space-between;
              gap: 12px; margin-bottom: 10px; }
  label.row span { color: #b6bcc6; font-size: 14px; }
  select, input { background: #23262e; color: #e6e6e6; border: 1px solid #333844;
                  border-radius: 7px; padding: 7px 9px; font: inherit; min-width: 150px; }
  select:disabled, input:disabled { opacity: .45; }
  .hint { font-size: 12px; color: #8b93a1; margin: -6px 0 10px; }
  .actions { display: flex; gap: 10px; align-items: center; }
  button.primary { flex: 1; padding: 12px; border-radius: 9px; border: 0; cursor: pointer;
                   background: #2f6fd0; color: #fff; font: inherit; font-weight: 600; }
  button.primary:hover { background: #3a7de0; }
  a.view { padding: 12px 16px; border-radius: 9px; background: #23262e; color: #cfe0f7;
           text-decoration: none; border: 1px solid #333844; white-space: nowrap; }
  /* An IP is long; the arrow was eating the first octets. */
  #address { min-width: 200px; }
  #note { margin-top: 12px; font-size: 13px; color: #8b93a1; min-height: 20px; }
  #note.ok { color: #7ddc9a; }
  #note.bad { color: #ff9b9b; }
</style>
</head>
<body>
<main>
  <h1>OpenScreen</h1>
  <p class="sub">共享这台电脑的屏幕。这里的改动立即生效，不用重启程序。</p>

  <section>
    <h2>传输方式</h2>
    <div class="modes" id="modes"></div>
  </section>

  <section>
    <h2>画面</h2>
    <label class="row"><span>共享哪块屏幕</span>
      <select id="screen"></select>
    </label>
    <div id="capturing" class="hint"></div>
    <label class="row"><span>分辨率</span>
      <select id="height">
        <option value="0">原生（最清晰）</option>
        <option value="2160">2160p（4K）</option>
        <option value="1600">1600p</option>
        <option value="1440">1440p</option>
        <option value="1080">1080p</option>
        <option value="720">720p</option>
      </select>
    </label>
    <label class="row"><span>帧率</span>
      <select id="fps">
        <option value="30">30 帧</option>
        <option value="60">60 帧</option>
        <option value="120">120 帧（需要 120Hz 屏幕）</option>
      </select>
    </label>
    <label class="row"><span>码率（kbps）</span>
      <input id="bitrate" type="number" min="1000" max="60000" step="1000">
    </label>
    <label class="row"><span>JPEG 质量（仅 MJPEG）</span>
      <input id="quality" type="number" min="40" max="95" step="1">
    </label>
    <label class="row"><span>H.264 Profile（仅 WebRTC）</span>
      <select id="profile">
        <option value="baseline">baseline（兼容性最好）</option>
        <option value="main">main</option>
        <option value="high">high（压缩率最好）</option>
      </select>
    </label>
  </section>

  <section>
    <h2>另一台设备打开</h2>
    <label class="row"><span>访问地址</span>
      <select id="address"></select>
    </label>
    <div id="note">两台设备必须在同一个局域网内。</div>
  </section>

  <section>
    <div class="actions">
      <button class="primary" id="apply">应用设置</button>
      <a class="view" id="view" href="/view">打开画面</a>
    </div>
  </section>
</main>
<script>
(() => {
  const MODES = [
    { id: 'webrtc', name: 'WebRTC', hint: '延迟最低，UDP 传输' },
    { id: 'mse', name: 'MSE', hint: '走 TCP，网络限制多时用' },
    { id: 'mjpeg', name: 'MJPEG', hint: '不依赖硬件编码器' },
    { id: 'h264', name: 'WebCodecs', hint: '仅本机或 HTTPS' }
  ];

  const modes = document.getElementById('modes');
  const screen = document.getElementById('screen');
  const height = document.getElementById('height');
  const fps = document.getElementById('fps');
  const bitrate = document.getElementById('bitrate');
  const quality = document.getElementById('quality');
  const profile = document.getElementById('profile');
  const address = document.getElementById('address');
  const note = document.getElementById('note');
  const view = document.getElementById('view');

  let current = 'webrtc';

  function paint() {
    modes.innerHTML = '';

    MODES.forEach((mode) => {
      const button = document.createElement('button');
      button.className = 'mode' + (mode.id === current ? ' on' : '');
      button.innerHTML = '<b>' + mode.name + '</b><span>' + mode.hint + '</span>';
      button.onclick = () => { current = mode.id; paint(); };
      modes.appendChild(button);
    });

    // Settings that do not apply to the chosen transport only look broken when left enabled.
    const jpeg = current === 'mjpeg';
    bitrate.disabled = jpeg;
    quality.disabled = !jpeg;
    profile.disabled = current !== 'webrtc';
  }

  function say(text, state) {
    note.textContent = text;
    note.className = state || '';
  }

  function showAddress() {
    const port = location.port || '80';
    view.href = 'http://' + (address.value || location.hostname) + ':' + port + '/view';
  }

  function describe(item) {
    let text = item.width + ' × ' + item.height;

    if (item.primary) {
      text += '（主屏）';
    } else if (item.index === 1) {
      text += '（第二屏）';
    }

    return text;
  }

  async function load() {
    const settings = await (await fetch('/settings', { cache: 'no-store' })).json();

    current = settings.mode;
    height.value = String(settings.height);
    fps.value = String(settings.fps);
    bitrate.value = settings.bitrate;
    quality.value = settings.quality;
    profile.value = settings.profile || 'baseline';

    screen.innerHTML = '';

    const auto = document.createElement('option');
    auto.value = 'auto';
    auto.textContent = '自动（能抓的那块）';
    screen.appendChild(auto);

    (settings.screens || []).forEach((item) => {
      const option = document.createElement('option');
      option.value = String(item.index);
      option.textContent = describe(item);
      screen.appendChild(option);
    });

    screen.value = settings.screen === null || settings.screen === undefined
      ? 'auto'
      : String(settings.screen);

    address.innerHTML = '';
    (settings.addresses || []).forEach((item) => {
      const option = document.createElement('option');
      option.value = item;
      option.textContent = item;
      address.appendChild(option);
    });

    // Which monitor is really being captured: the GPU refuses to duplicate some of them, and the
    // viewer would otherwise be looking at a screen nobody asked for.
    const label = (index) => {
      const item = (settings.screens || []).find((candidate) => candidate.index === index);
      return item ? describe(item) : '#' + index;
    };

    const capturing = document.getElementById('capturing');
    capturing.textContent = settings.capturing === null || settings.capturing === undefined
      ? '当前没有在抓屏（等观看端连接）'
      : '实际抓取：' + label(settings.capturing) +
        (settings.viewers ? '　观看中：' + settings.viewers : '');

    paint();
    showAddress();
  }

  address.addEventListener('change', showAddress);

  document.getElementById('apply').onclick = async () => {
    const body = new URLSearchParams({
      mode: current,
      screen: screen.value,
      height: height.value,
      fps: fps.value,
      bitrate: bitrate.value,
      quality: quality.value,
      profile: profile.value
    });

    say('正在应用…');

    try {
      const response = await fetch('/settings', { method: 'POST', body });

      if (!response.ok) {
        say('应用失败：' + (await response.text()), 'bad');
        return;
      }

      // No navigation: the panel is what you keep open while tuning.
      say('设置已应用。观看页面会自动刷新生效。', 'ok');
    } catch (error) {
      say('应用失败：' + error.message, 'bad');
    }
  };

  load().catch((error) => say('读不到设置：' + error.message, 'bad'));
})();
</script>
</body>
</html>
"##;
