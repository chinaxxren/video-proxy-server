const player = document.querySelector('#player');
const results = document.querySelector('#results');
const connectionStatus = document.querySelector('#connection-status');
const runAllButton = document.querySelector('#run-all');
const runRangeButton = document.querySelector('#run-range');

const formatTime = (seconds) => {
  if (!Number.isFinite(seconds)) return '--:--';
  const minutes = Math.floor(seconds / 60);
  return `${minutes}:${String(Math.floor(seconds % 60)).padStart(2, '0')}`;
};

const updatePlayerStats = () => {
  document.querySelector('#position').textContent = formatTime(player.currentTime);
  document.querySelector('#duration').textContent = formatTime(player.duration);
  let end = 0;
  for (let i = 0; i < player.buffered.length; i += 1) end = Math.max(end, player.buffered.end(i));
  const percent = player.duration > 0 ? Math.min(100, end / player.duration * 100) : 0;
  document.querySelector('#buffered').textContent = `${percent.toFixed(0)}%`;
};

['loadedmetadata', 'durationchange', 'progress', 'timeupdate', 'seeked'].forEach((event) => {
  player.addEventListener(event, updatePlayerStats);
});

player.addEventListener('loadedmetadata', () => {
  connectionStatus.textContent = 'Proxy media ready';
  connectionStatus.className = 'status ready';
});
player.addEventListener('error', () => {
  connectionStatus.textContent = 'Media request failed';
  connectionStatus.className = 'status error';
});

const addResult = ({ name, ok, status, range, bytes, elapsed }) => {
  results.querySelector('.empty')?.remove();
  const row = document.createElement('tr');
  const values = [name, `${ok ? 'PASS' : 'FAIL'} (${status})`, range || '-', `${bytes} B`, `${elapsed.toFixed(1)} ms`];
  values.forEach((value, index) => {
    const cell = document.createElement('td');
    cell.textContent = value;
    if (index === 1) cell.className = ok ? 'pass' : 'fail';
    row.appendChild(cell);
  });
  results.prepend(row);
};

async function requestRange(name, start, end) {
  const before = performance.now();
  try {
    const response = await fetch('/playback', { headers: { Range: `bytes=${start}-${end}` }, cache: 'no-store' });
    const body = await response.arrayBuffer();
    const expected = end - start + 1;
    const contentRange = response.headers.get('content-range') || '';
    const ok = response.status === 206 && body.byteLength === expected && contentRange.startsWith(`bytes ${start}-${end}/`);
    addResult({ name, ok, status: response.status, range: contentRange, bytes: body.byteLength, elapsed: performance.now() - before });
    return ok;
  } catch (error) {
    addResult({ name, ok: false, status: 'network', range: error.message, bytes: 0, elapsed: performance.now() - before });
    return false;
  }
}

async function runAll() {
  runAllButton.disabled = true;
  const checks = [
    ['Initial range', 0, 65535],
    ['Repeated cache range', 0, 65535],
    ['Seek range', 1048576, 1114111],
    ['Overlapping range', 32768, 98303],
  ];
  for (const [name, start, end] of checks) await requestRange(name, start, end);
  runAllButton.disabled = false;
}

runAllButton.addEventListener('click', runAll);
runRangeButton.addEventListener('click', () => {
  const start = Number(document.querySelector('#range-start').value);
  const end = Number(document.querySelector('#range-end').value);
  if (!Number.isSafeInteger(start) || !Number.isSafeInteger(end) || start < 0 || end < start) {
    addResult({ name: 'Custom range', ok: false, status: 'input', range: 'Invalid byte range', bytes: 0, elapsed: 0 });
    return;
  }
  requestRange('Custom range', start, end);
});

document.querySelector('#clear-log').addEventListener('click', () => {
  results.replaceChildren();
  const row = document.createElement('tr');
  row.className = 'empty';
  row.innerHTML = '<td colspan="5">No checks run</td>';
  results.appendChild(row);
});

const hlsPlayer = document.querySelector('#hls-player');
const hlsStatus = document.querySelector('#hls-status');
const loadHlsButton = document.querySelector('#load-hls');
const repeatSegmentButton = document.querySelector('#repeat-segment');
let firstHlsSegment = null;
let hlsObjectUrl = null;

const waitForEvent = (target, name) => new Promise((resolve, reject) => {
  const cleanup = () => {
    target.removeEventListener(name, onSuccess);
    target.removeEventListener('error', onError);
  };
  const onSuccess = (event) => {
    cleanup();
    resolve(event);
  };
  const onError = () => {
    cleanup();
    reject(new Error(`${name} failed`));
  };
  target.addEventListener(name, onSuccess, { once: true });
  target.addEventListener('error', onError, { once: true });
});

const appendBuffer = (sourceBuffer, bytes) => new Promise((resolve, reject) => {
  const cleanup = () => {
    sourceBuffer.removeEventListener('updateend', onSuccess);
    sourceBuffer.removeEventListener('error', onError);
  };
  const onSuccess = () => {
    cleanup();
    resolve();
  };
  const onError = () => {
    cleanup();
    reject(new Error('MediaSource append failed'));
  };
  sourceBuffer.addEventListener('updateend', onSuccess, { once: true });
  sourceBuffer.addEventListener('error', onError, { once: true });
  sourceBuffer.appendBuffer(bytes);
});

const releaseHlsPlayer = () => {
  hlsPlayer.pause();
  hlsPlayer.removeAttribute('src');
  hlsPlayer.load();
  if (hlsObjectUrl) URL.revokeObjectURL(hlsObjectUrl);
  hlsObjectUrl = null;
};

const setHlsStatus = (text, state = '') => {
  hlsStatus.textContent = text;
  hlsStatus.className = `status ${state}`.trim();
};

async function fetchHlsResource(url) {
  const response = await fetch(url, { cache: 'no-store' });
  if (!response.ok) throw new Error(`${url} returned ${response.status}`);
  return { response, bytes: await response.arrayBuffer() };
}

async function loadHls() {
  releaseHlsPlayer();
  loadHlsButton.disabled = true;
  repeatSegmentButton.disabled = true;
  setHlsStatus('Loading playlist');
  try {
    const playlistResponse = await fetch('/hls-playback', { cache: 'no-store' });
    if (!playlistResponse.ok) throw new Error(`Playlist returned ${playlistResponse.status}`);
    const playlistType = playlistResponse.headers.get('content-type') || '';
    const playlist = await playlistResponse.text();
    const mapMatch = playlist.match(/#EXT-X-MAP:URI="([^"]+)"/);
    const segments = playlist.split(/\r?\n/).map(line => line.trim()).filter(line => line && !line.startsWith('#'));
    const rewritten = [mapMatch?.[1], ...segments].filter(url => url?.startsWith('/proxy/'));
    const playlistOk = playlistType.includes('mpegurl') && Boolean(mapMatch) && segments.length > 0 && rewritten.length === segments.length + 1;
    document.querySelector('#hls-playlist-check').textContent = playlistOk ? 'PASS' : 'FAIL';
    document.querySelector('#hls-playlist-check').className = playlistOk ? 'pass' : 'fail';
    document.querySelector('#hls-rewrite-count').textContent = String(rewritten.length);
    document.querySelector('#hls-segment-count').textContent = `0 / ${segments.length}`;
    if (!playlistOk) throw new Error('Playlist MIME or URI rewrite check failed');

    const mime = 'video/mp4; codecs="avc1.64001f, mp4a.40.2"';
    if (!window.MediaSource || !MediaSource.isTypeSupported(mime)) throw new Error(`Unsupported MediaSource type: ${mime}`);
    const mediaSource = new MediaSource();
    hlsObjectUrl = URL.createObjectURL(mediaSource);
    hlsPlayer.src = hlsObjectUrl;
    await waitForEvent(mediaSource, 'sourceopen');
    const sourceBuffer = mediaSource.addSourceBuffer(mime);

    let loadedBytes = 0;
    const resources = [mapMatch[1], ...segments];
    firstHlsSegment = segments[0];
    for (let index = 0; index < resources.length; index += 1) {
      setHlsStatus(index === 0 ? 'Loading init segment' : `Loading segment ${index} / ${segments.length}`);
      const resource = await fetchHlsResource(resources[index]);
      loadedBytes += resource.bytes.byteLength;
      await appendBuffer(sourceBuffer, resource.bytes);
      document.querySelector('#hls-loaded-bytes').textContent = `${loadedBytes.toLocaleString()} B`;
      if (index > 0) document.querySelector('#hls-segment-count').textContent = `${index} / ${segments.length}`;
    }
    mediaSource.endOfStream();
    setHlsStatus('HLS media ready', 'ready');
    repeatSegmentButton.disabled = false;
  } catch (error) {
    setHlsStatus(error.message, 'error');
  } finally {
    loadHlsButton.disabled = false;
  }
}

loadHlsButton.addEventListener('click', loadHls);
repeatSegmentButton.addEventListener('click', async () => {
  if (!firstHlsSegment) return;
  repeatSegmentButton.disabled = true;
  const before = performance.now();
  try {
    const resource = await fetchHlsResource(firstHlsSegment);
    setHlsStatus(`First segment cached: ${resource.bytes.byteLength.toLocaleString()} B in ${(performance.now() - before).toFixed(1)} ms`, 'ready');
  } catch (error) {
    setHlsStatus(error.message, 'error');
  } finally {
    repeatSegmentButton.disabled = false;
  }
});
