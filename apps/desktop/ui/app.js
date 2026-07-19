const openFlights = new Set();
let selectedSessionId = null;

const toast = (message) => {
  const node = document.querySelector('#toast');
  node.textContent = message;
  node.classList.add('show');
  window.setTimeout(() => node.classList.remove('show'), 2600);
};

const request = async (url, body) => {
  const response = await fetch(url, {
    method: 'POST', headers: body !== undefined ? { 'content-type': 'application/json' } : {},
    body: body !== undefined ? JSON.stringify(body) : undefined,
  });
  const result = await response.json();
  if (result.error) throw new Error(result.error);
  return result;
};

const refreshSharedFrames = () => htmx.trigger('#share-tray', 'refreshShared');

const ensurePrivacyConsent = async () => {
  const dialog = document.querySelector('#consent-dialog');
  try {
    const response = await fetch('/api/consent');
    const status = await response.json();
    if (status.error) throw new Error(status.error);
    if (!status.accepted && !dialog.open) dialog.showModal();
  } catch (error) {
    toast(`Privacy status unavailable: ${error.message}`);
  }
};

const syncRecorderMode = () => {
  const state = document.querySelector('#status [data-recorder-state]')?.dataset.recorderState;
  document.body.classList.toggle('recording-mode', state === 'Recording');
  if (state === 'Recording') document.querySelector('#preferences-dialog')?.close();
};

const openPreferences = async () => {
  const dialog = document.querySelector('#preferences-dialog');
  const content = document.querySelector('#preferences-content');
  try {
    const response = await fetch('/partials/preferences');
    content.innerHTML = await response.text();
    htmx.process(content);
    dialog.showModal();
    content.querySelector('button, input:not([type="hidden"]), summary')?.focus();
  } catch (error) {
    toast(error.message);
  }
};

const setPreferencePath = (kind, value) => {
  const form = document.querySelector('[data-preferences-form]');
  if (!form || !value) return;
  form.elements[`${kind}_root`].value = value;
  form.querySelector(`[data-path-output="${kind}"]`).textContent = value;
};

const applyArchiveState = () => {
  const collapsed = window.localStorage.getItem('cdxvidext.archiveCollapsed') === 'true';
  document.body.classList.toggle('archive-collapsed', collapsed);
  const button = document.querySelector('[data-toggle-archive]');
  if (button) {
    button.setAttribute('aria-expanded', String(!collapsed));
    button.setAttribute('aria-label', collapsed ? 'Expand recorded flights' : 'Collapse recorded flights');
  }
};

const restoreFlightState = () => {
  document.querySelectorAll('[data-flight-card]').forEach((card) => {
    const id = card.dataset.flightCard;
    const open = openFlights.has(id);
    card.classList.toggle('open', open);
    card.classList.toggle('selected', id === selectedSessionId);
    card.querySelector('[data-flight-details]')?.toggleAttribute('hidden', !open);
    card.querySelector('[data-flight-toggle]')?.setAttribute('aria-expanded', String(open));
  });
};

const reloadSelectedReview = () => {
  if (selectedSessionId) {
    htmx.ajax('GET', `/partials/timeline/${selectedSessionId}`, { target: '#reviewer', swap: 'innerHTML' });
  }
};

const seekToEvent = (offsetMs, eventKey) => {
  const video = document.querySelector('#flight-video');
  if (!video) return;
  video.pause();
  document.querySelector('[data-play]')?.replaceChildren('Play');
  video.addEventListener('seeked', () => video.pause(), { once: true });
  video.currentTime = Math.max(0, Number(offsetMs)) / 1000;
  document.querySelectorAll('.telemetry-event.active, .timeline-marker.active').forEach((node) => node.classList.remove('active'));
  document.querySelector(`.telemetry-event[data-event-key="${CSS.escape(eventKey)}"]`)?.classList.add('active');
  document.querySelector(`.timeline-marker[data-event-key="${CSS.escape(eventKey)}"]`)?.classList.add('active');
};

const renderMarkers = async (sessionId, durationMs) => {
  const svg = document.querySelector('#timeline-markers');
  if (!svg || !durationMs) return;
  try {
    const response = await fetch(`/api/timeline-map/${sessionId}`);
    const timeline = await response.json();
    if (timeline.error) throw new Error(timeline.error);
    svg.replaceChildren();
    timeline.categories.forEach((category, categoryIndex) => {
      category.events.forEach((item) => {
        const start = Math.max(0, Math.min(1000, item.start_offset_100ns / 10000 / durationMs * 1000));
        const end = Math.max(start, Math.min(1000, item.end_offset_100ns / 10000 / durationMs * 1000));
        const marker = document.createElementNS('http://www.w3.org/2000/svg', end - start > 1 ? 'rect' : 'line');
        marker.classList.add('timeline-marker');
        marker.dataset.eventKey = item.event_key;
        marker.style.setProperty('--marker-color', item.color);
        if (marker.tagName === 'rect') {
          marker.setAttribute('x', String(start));
          marker.setAttribute('width', String(Math.max(2, end - start)));
          marker.setAttribute('y', String(3 + categoryIndex * 4));
          marker.setAttribute('height', '3');
          marker.setAttribute('rx', '1');
        } else {
          marker.setAttribute('x1', String(end));
          marker.setAttribute('x2', String(end));
          marker.setAttribute('y1', '3');
          marker.setAttribute('y2', '27');
        }
        marker.addEventListener('click', () => seekToEvent(item.seek_offset_ms, item.event_key));
        svg.append(marker);
      });
    });
  } catch (error) {
    toast(error.message);
  }
};

document.addEventListener('click', async (event) => {
  const target = event.target;
  const arm = target.closest('[data-arm]');
  const pin = target.closest('[data-pin-session]');
  const remove = target.closest('[data-delete-session]');
  const preferencesOpen = target.closest('[data-open-preferences]');
  const preferencesClose = target.closest('[data-close-preferences]');
  const browseRoot = target.closest('[data-browse-root]');
  const resetRoot = target.closest('[data-reset-root]');
  const sharedRemove = target.closest('[data-remove-shared]');
  const sharedClear = target.closest('[data-clear-shared]');
  const sharedPreview = target.closest('[data-shared-preview]');
  const archiveToggle = target.closest('[data-toggle-archive]');
  const flightToggle = target.closest('[data-flight-toggle]');
  const flightSelect = target.closest('[data-select-session]');
  const rename = target.closest('[data-rename-flight]');
  const saveRename = target.closest('[data-save-rename]');
  const resetRename = target.closest('[data-reset-rename]');
  const cancelRename = target.closest('[data-cancel-rename]');
  const eventSeek = target.closest('[data-event-seek]');
  const eventToggle = target.closest('[data-event-toggle]');
  const shareScreenshot = target.closest('[data-share-screenshot]');
  const acceptConsent = target.closest('[data-accept-consent]');

  if (acceptConsent) {
    const errorNode = document.querySelector('[data-consent-error]');
    try {
      await request('/api/consent');
      document.querySelector('#consent-dialog')?.close();
      toast('Privacy notice accepted. You can now arm a monitor.');
    } catch (error) {
      errorNode.textContent = error.message;
      errorNode.hidden = false;
    }
  }

  if (preferencesOpen) openPreferences();
  if (preferencesClose) document.querySelector('#preferences-dialog')?.close();
  if (browseRoot) {
    const kind = browseRoot.dataset.browseRoot;
    const current = document.querySelector('[data-preferences-form]')?.elements[`${kind}_root`]?.value ?? '';
    try {
      const result = await request('/api/preferences/browse', { current });
      if (result.path) setPreferencePath(kind, result.path);
    } catch (error) { toast(error.message); }
  }
  if (resetRoot) setPreferencePath(resetRoot.dataset.resetRoot, resetRoot.dataset.defaultRoot);

  if (archiveToggle) {
    const collapsed = !document.body.classList.contains('archive-collapsed');
    window.localStorage.setItem('cdxvidext.archiveCollapsed', String(collapsed));
    applyArchiveState();
  }
  if (flightToggle) {
    event.stopPropagation();
    const id = flightToggle.dataset.flightToggle;
    openFlights.has(id) ? openFlights.delete(id) : openFlights.add(id);
    restoreFlightState();
  }
  if (flightSelect) {
    selectedSessionId = flightSelect.dataset.selectSession;
    restoreFlightState();
  }
  if (arm) {
    const index = document.querySelector('#monitor-select')?.value;
    try { await request(`/api/arm/${index}`); toast('Recorder armed. The next prompt starts capture.'); } catch (error) { toast(error.message); }
  }
  if (pin) {
    event.stopPropagation();
    try { await request(`/api/pin/${pin.dataset.pinSession}/${pin.dataset.pinned !== 'true'}`); htmx.trigger('#sessions', 'refresh'); } catch (error) { toast(error.message); }
  }
  if (rename) {
    document.querySelector(`[data-rename-form="${CSS.escape(rename.dataset.renameFlight)}"]`)?.toggleAttribute('hidden');
  }
  if (cancelRename) {
    document.querySelector(`[data-rename-form="${CSS.escape(cancelRename.dataset.cancelRename)}"]`)?.setAttribute('hidden', '');
  }
  if (saveRename) {
    const id = saveRename.dataset.saveRename;
    const value = document.querySelector(`[data-rename-form="${CSS.escape(id)}"] input`)?.value ?? '';
    try { await request(`/api/rename/${id}`, { display_name: value }); htmx.trigger('#sessions', 'refresh'); reloadSelectedReview(); toast('Flight renamed.'); } catch (error) { toast(error.message); }
  }
  if (resetRename) {
    const id = resetRename.dataset.resetRename;
    try { await request(`/api/rename/${id}`, { display_name: null }); htmx.trigger('#sessions', 'refresh'); reloadSelectedReview(); toast('Timestamp title restored.'); } catch (error) { toast(error.message); }
  }
  if (remove) {
    event.stopPropagation();
    const id = remove.dataset.deleteSession;
    const title = remove.dataset.flightTitle;
    const pinned = remove.dataset.pinned === 'true';
    const warning = pinned
      ? `Delete pinned flight “${title}”? It will be unpinned and permanently removed.`
      : `Permanently delete flight “${title}”?`;
    if (window.confirm(warning)) {
      try {
        await request(`/api/delete/${id}`, { delete_pinned: pinned });
        openFlights.delete(id);
        if (selectedSessionId === id) {
          selectedSessionId = null;
          document.querySelector('#reviewer').innerHTML = '<div class="review-empty"><h2>Flight deleted</h2></div>';
          document.querySelector('[data-share-screenshot]').disabled = true;
        }
        htmx.trigger('#sessions', 'refresh'); refreshSharedFrames(); toast('Flight deleted.');
      } catch (error) { toast(error.message); }
    }
  }
  if (sharedRemove) {
    event.stopPropagation();
    try { await request(`/api/shared/${sharedRemove.dataset.removeShared}/remove`); refreshSharedFrames(); toast('Screenshot removed.'); } catch (error) { toast(error.message); }
  }
  if (sharedClear && window.confirm('Clear all shared screenshots? The recordings remain available.')) {
    try { await request('/api/shared/clear'); refreshSharedFrames(); toast('Shared screenshots cleared.'); } catch (error) { toast(error.message); }
  }
  if (sharedPreview) {
    const dialog = document.querySelector('#shared-frame-dialog');
    dialog.querySelector('img').src = `/api/shared/${sharedPreview.dataset.sharedPreview}/image`;
    dialog.querySelector('[data-dialog-time]').textContent = sharedPreview.dataset.sharedTime;
    dialog.querySelector('[data-dialog-event]').textContent = sharedPreview.dataset.sharedEvent;
    dialog.querySelector('[data-dialog-session]').textContent = sharedPreview.dataset.sharedSession;
    dialog.showModal();
  }
  if (eventSeek) seekToEvent(eventSeek.dataset.eventSeek, eventSeek.dataset.eventKey);
  if (eventToggle) {
    event.stopPropagation();
    const key = eventToggle.dataset.eventToggle;
    const details = document.querySelector(`[data-event-details="${CSS.escape(key)}"]`);
    const opening = details.hasAttribute('hidden');
    if (opening && !details.dataset.loaded) {
      try {
        const response = await fetch(eventToggle.dataset.eventDetailUrl);
        details.innerHTML = await response.text();
        details.dataset.loaded = 'true';
      } catch (error) { toast(error.message); return; }
    }
    details.toggleAttribute('hidden', !opening);
    eventToggle.setAttribute('aria-expanded', String(opening));
  }
  if (shareScreenshot && !shareScreenshot.disabled) {
    const video = document.querySelector('#flight-video');
    const shell = video?.closest('[data-session-id]');
    if (video && shell) {
      try {
        const result = await request('/api/select', { session_id: shell.dataset.sessionId, offset_ms: Math.round(video.currentTime * 1000) });
        refreshSharedFrames(); toast(`Screenshot shared · ${result.count} ready for Codex.`);
      } catch (error) { toast(error.message); }
    }
  }
});

document.addEventListener('change', (event) => {
  if (event.target.matches('[data-consent-checkbox]')) {
    document.querySelector('[data-accept-consent]').disabled = !event.target.checked;
  }
  const form = event.target.closest('[data-preferences-form]');
  if (!form) return;
  if (event.target.name === 'flight_retention_enabled') form.elements.flight_retention_days.disabled = !event.target.checked;
  if (event.target.name === 'snapshot_retention_enabled') form.elements.snapshot_retention_days.disabled = !event.target.checked;
  if (event.target.name === 'cutoff_enabled') {
    form.elements.cutoff_minutes.disabled = !event.target.checked;
    form.elements.cutoff_seconds.disabled = !event.target.checked;
  }
});

document.addEventListener('submit', async (event) => {
  const form = event.target.closest('[data-preferences-form]');
  if (!form) return;
  event.preventDefault();
  const values = new FormData(form);
  const cutoffEnabled = form.elements.cutoff_enabled.checked;
  const cutoffSeconds = Number(form.elements.cutoff_minutes.value) * 60 + Number(form.elements.cutoff_seconds.value);
  const preferences = {
    flight_root: values.get('flight_root'),
    snapshot_root: values.get('snapshot_root'),
    flight_retention: {
      enabled: form.elements.flight_retention_enabled.checked,
      days: Number(form.elements.flight_retention_days.value),
      applies_after_utc: null,
    },
    snapshot_retention: {
      enabled: form.elements.snapshot_retention_enabled.checked,
      days: Number(form.elements.snapshot_retention_days.value),
      applies_after_utc: null,
    },
    cutoff_seconds: cutoffEnabled ? cutoffSeconds : null,
    quality: values.get('quality'),
    resolution: values.get('resolution'),
  };
  try {
    await request('/api/preferences', preferences);
    document.querySelector('#preferences-dialog')?.close();
    toast('Preferences saved. New flights will use these settings.');
  } catch (error) {
    const errorNode = form.querySelector('[data-preferences-error]');
    errorNode.textContent = error.message;
    errorNode.hidden = false;
    errorNode.focus();
  }
});

const bindReview = () => {
  const video = document.querySelector('#flight-video');
  if (!video || video.dataset.bound) return;
  video.dataset.bound = 'true';
  const scrub = document.querySelector('#timeline-scrub');
  const playhead = document.querySelector('#playhead');
  const shell = video.closest('[data-session-id]');
  const share = document.querySelector('[data-share-screenshot]');
  share.disabled = false;
  share.dataset.sessionId = shell.dataset.sessionId;
  const format = (seconds) => {
    const ms = Math.max(0, Math.round(seconds * 1000));
    return `${String(Math.floor(ms / 60000)).padStart(2, '0')}:${String(Math.floor(ms / 1000) % 60).padStart(2, '0')}.${String(ms % 1000).padStart(3, '0')}`;
  };
  video.addEventListener('timeupdate', () => { scrub.value = Math.round(video.currentTime * 1000); playhead.value = format(video.currentTime); });
  scrub.addEventListener('input', () => { video.pause(); video.currentTime = Number(scrub.value) / 1000; });
  document.querySelector('[data-play]')?.addEventListener('click', (click) => { if (video.paused) { video.play(); click.target.textContent = 'Pause'; } else { video.pause(); click.target.textContent = 'Play'; } });
  document.querySelectorAll('[data-step]').forEach((button) => button.addEventListener('click', () => { video.pause(); video.currentTime = Math.max(0, video.currentTime + Number(button.dataset.step) / 30); }));
  renderMarkers(shell.dataset.sessionId, Number(scrub.max));
};

document.body.addEventListener('htmx:afterSwap', (event) => {
  if (event.target.id === 'sessions') restoreFlightState();
  if (event.target.id === 'status') syncRecorderMode();
  bindReview();
});
document.addEventListener('DOMContentLoaded', () => {
  applyArchiveState(); restoreFlightState(); syncRecorderMode(); bindReview(); ensurePrivacyConsent();
  document.querySelector('#consent-dialog')?.addEventListener('cancel', (event) => event.preventDefault());
});
