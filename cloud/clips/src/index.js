// Shared clips at clips.hindsighthq.app.
//   GET  /c/<id>                 the share page, with the tags Discord reads
//   GET  /v/<id>.mp4             the video     GET /t/<id>.jpg  its thumbnail
//   POST /api/clips              upload, from the app
//   PUT  /api/clips/<id>/thumb   thumbnail, with the clip's delete token
//   DELETE /api/clips/<id>       unshare, with the clip's delete token
//   GET/POST /api/clips/<id>/reactions, POST /api/clips/<id>/report
//
// Cloudflare has no spending cap, so the limits live here: a conditional update
// reserves storage before anything is written, and the Worker stops serving
// before the month's included requests run out. Files stay on the 14-day
// lifecycle rule even if the cleanup below never ran.

import {
  COUNT_EVERY, DAILY_CAP_BYTES, EMOJI, HIDE_AFTER_REPORTS, KEEP_DAYS, MAX_CLIP_BYTES, MAX_THUMB_BYTES, MONTHLY_REQUEST_CAP, REPORT_REASONS,
  TOTAL_CAP_BYTES, UPLOADS_PER_DAY, cleanText, esc, headerText, isJpeg, isMp4, newId, number, parseRange, validId,
} from "./rules.js";

const DAY_MS = 86_400_000;
// Takedowns reach every data centre within this long.
const CACHE_SECS = 600;

const json = (body, status = 200) => new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json", "cache-control": "no-store" } });
const refuse = (error, status) => json({ error }, status);
const today = () => new Date().toISOString().slice(0, 10);
const month = () => new Date().toISOString().slice(0, 7);

async function sha256(text) {
  const hash = await crypto.subtle.digest("SHA-256", new TextEncoder().encode(text));
  return [...new Uint8Array(hash)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

const randomId = () => newId(crypto.getRandomValues(new Uint8Array(10)));
const randomToken = () => [...crypto.getRandomValues(new Uint8Array(24))].map((b) => b.toString(16).padStart(2, "0")).join("");
const ipOf = (req) => req.headers.get("cf-connecting-ip") || "unknown";

/* ---------- the monthly brake ---------- */

let brake = { checked: 0, stopped: false };

async function stopped(env) {
  if (Date.now() - brake.checked > 60_000) {
    const row = await env.DB.prepare("SELECT requests FROM request_months WHERE month = ?").bind(month()).first();
    brake = { checked: Date.now(), stopped: (row?.requests ?? 0) >= MONTHLY_REQUEST_CAP };
  }
  return brake.stopped;
}

function count(env, ctx) {
  if (Math.random() * COUNT_EVERY >= 1) return;
  ctx.waitUntil(
    env.DB.prepare("INSERT INTO request_months (month, requests) VALUES (?1, ?2) ON CONFLICT (month) DO UPDATE SET requests = requests + ?2").bind(month(), COUNT_EVERY).run(),
  );
}

/* ---------- pages and files ---------- */

const htmlHeaders = (extra = {}) => ({
  "content-type": "text/html; charset=utf-8",
  "content-security-policy": "default-src 'none'; img-src 'self'; media-src 'self'; style-src 'self'; script-src 'self'; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
  "x-content-type-options": "nosniff",
  "referrer-policy": "no-referrer",
  ...extra,
});

const shell = (title, head, body, status = 200) =>
  new Response(
    `<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1"><title>${esc(title)}</title>
<meta name="theme-color" content="#e5484d"><link rel="icon" href="/favicon-32.png"><link rel="stylesheet" href="/app.css">${head}</head>
<body><header><a class="brand" href="https://hindsighthq.app">Hindsight</a><a class="get" href="https://hindsighthq.app">Get Hindsight</a></header><main>${body}</main>
<footer><a href="/terms">Terms</a><a href="https://hindsighthq.app/impressum">Impressum</a><a href="https://hindsighthq.app/datenschutz">Datenschutz</a><span>Clips are deleted after ${KEEP_DAYS} days</span></footer></body></html>`,
    { status, headers: htmlHeaders({ "cache-control": "no-store" }) },
  );

const gone = () => shell("Clip not found", "", `<div class="gone"><h1>This clip isn't here</h1><p>It may have expired, or the person who shared it removed it.</p></div>`, 404);

async function visibleClip(env, id) {
  if (!validId(id)) return null;
  const clip = await env.DB.prepare("SELECT * FROM clips WHERE id = ?").bind(id).first();
  return clip && !clip.hidden && clip.expires_at > Date.now() ? clip : null;
}

async function sharePage(env, url, id) {
  const clip = await visibleClip(env, id);
  if (!clip) return gone();
  const origin = url.origin;
  const video = `${origin}/v/${id}.mp4`;
  const image = clip.has_thumb ? `${origin}/t/${id}.jpg` : "";
  const description = [clip.game, "shared with Hindsight"].filter(Boolean).join(" · ");
  const head = `
<meta property="og:site_name" content="Hindsight"><meta property="og:type" content="video.other">
<meta property="og:title" content="${esc(clip.title)}"><meta property="og:description" content="${esc(description)}">
<meta property="og:url" content="${esc(`${origin}/c/${id}`)}">
<meta property="og:video" content="${esc(video)}"><meta property="og:video:url" content="${esc(video)}"><meta property="og:video:secure_url" content="${esc(video)}">
<meta property="og:video:type" content="video/mp4"><meta property="og:video:width" content="${clip.width}"><meta property="og:video:height" content="${clip.height}">
${image ? `<meta property="og:image" content="${esc(image)}"><meta name="twitter:image" content="${esc(image)}">` : ""}
<meta name="twitter:card" content="player"><meta name="twitter:title" content="${esc(clip.title)}">
<meta name="twitter:player" content="${esc(video)}"><meta name="twitter:player:width" content="${clip.width}"><meta name="twitter:player:height" content="${clip.height}">
<script src="/app.js" defer></script>`;
  const days = Math.max(1, Math.ceil((clip.expires_at - Date.now()) / DAY_MS));
  const reasons = REPORT_REASONS.map((r) => `<option>${esc(r)}</option>`).join("");
  const body = `<article data-clip="${esc(id)}">
<video src="/v/${esc(id)}.mp4" ${image ? `poster="/t/${esc(id)}.jpg"` : ""} controls playsinline preload="metadata" width="${clip.width}" height="${clip.height}"></video>
<div class="info"><div><h1>${esc(clip.title)}</h1><p>${esc(clip.game)}${clip.game ? " · " : ""}deleted in ${days} day${days === 1 ? "" : "s"}</p></div>
<div class="actions"><button class="copy" id="copy" type="button">Copy link</button>
<button class="flag" id="flag" type="button" title="Report this clip" aria-label="Report this clip"><svg viewBox="0 0 24 24" aria-hidden="true"><path d="M5 21V4M5 4h11l-2 4 2 4H5"/></svg></button></div></div>
<div class="reactions" id="reactions">${EMOJI.map((e) => `<button type="button" data-emoji="${e}" aria-pressed="false"><span>${e}</span><b>0</b></button>`).join("")}</div></article>
<dialog id="report-dialog" aria-labelledby="report-title"><form id="report">
<h2 id="report-title">Report this clip</h2>
<label>Reason<select name="reason">${reasons}</select></label>
<label>Details<textarea name="detail" maxlength="1000" rows="3" placeholder="Optional"></textarea></label>
<label>Contact, if you'd like a reply<input name="contact" maxlength="200" placeholder="Optional"></label>
<p id="report-result" role="status"></p>
<div class="dialog-buttons"><button type="button" class="secondary" id="report-cancel">Cancel</button><button type="submit" id="report-send">Send report</button></div>
</form></dialog>`;
  return shell(`${clip.title} · Hindsight`, head, body);
}

/** Whole files are cached, so views don't read R2; a Range is cut from the copy. */
async function serveFile(req, env, ctx, url, id, ext, type) {
  if (!validId(id)) return new Response("Not found", { status: 404 });
  const cache = caches.default;
  const key = new Request(`${url.origin}/${ext === "mp4" ? "v" : "t"}/${id}.${ext}`);
  const headers = { "content-type": type, "accept-ranges": "bytes", "cache-control": `public, max-age=${CACHE_SECS}`, "x-content-type-options": "nosniff" };
  const range = req.headers.get("range");
  const cached = await cache.match(new Request(key.url, { headers: range ? { range } : {} }));
  if (cached) return cached;

  const object = await env.CLIPS.get(`${id}.${ext}`);
  if (!object) return new Response("Not found", { status: 404 });
  const data = await object.arrayBuffer();
  ctx.waitUntil(cache.put(key, new Response(data, { headers: { ...headers, "content-length": String(data.byteLength) } })));
  const part = parseRange(range, data.byteLength);
  if (!part) return new Response(data, { headers: { ...headers, "content-length": String(data.byteLength) } });
  return new Response(data.slice(part.start, part.end + 1), {
    status: 206,
    headers: { ...headers, "content-length": String(part.end - part.start + 1), "content-range": `bytes ${part.start}-${part.end}/${data.byteLength}` },
  });
}

/* ---------- uploads ---------- */

const release = (env, bytes) =>
  env.DB.batch([
    env.DB.prepare("UPDATE usage SET bytes = MAX(0, bytes - ?) WHERE id = 1").bind(bytes),
    env.DB.prepare("UPDATE upload_days SET bytes = MAX(0, bytes - ?) WHERE day = ?").bind(bytes, today()),
  ]);

async function upload(req, env) {
  const tester = env.TEST_UPLOAD_TOKEN && req.headers.get("x-hindsight-test") === env.TEST_UPLOAD_TOKEN;
  if (env.UPLOADS_OPEN !== "true" && !tester) return refuse("Sharing isn't open yet", 503);

  const size = Number(req.headers.get("content-length") || 0);
  if (!size || size > MAX_CLIP_BYTES) return refuse(`Clips can be up to ${MAX_CLIP_BYTES / 1_000_000} MB`, 413);

  const day = today();
  const uploader = await sha256(`${env.HASH_SALT}|${day}|${ipOf(req)}`);
  const quota = await env.DB.prepare(
    "INSERT INTO uploader_days (day, uploader, uploads) VALUES (?1, ?2, 1) ON CONFLICT (day, uploader) DO UPDATE SET uploads = uploads + 1 WHERE uploads < ?3",
  ).bind(day, uploader, UPLOADS_PER_DAY).run();
  if (!quota.meta.changes) return refuse("You've reached today's share limit", 429);

  const held = await env.DB.prepare("UPDATE usage SET bytes = bytes + ?1 WHERE id = 1 AND bytes + ?1 <= ?2").bind(size, TOTAL_CAP_BYTES).run();
  if (!held.meta.changes) return refuse("Clip storage is full right now. Try again later", 507);
  const dayHeld = await env.DB.prepare(
    "INSERT INTO upload_days (day, bytes) VALUES (?1, ?2) ON CONFLICT (day) DO UPDATE SET bytes = bytes + ?2 WHERE bytes + ?2 <= ?3",
  ).bind(day, size, DAILY_CAP_BYTES).run();
  if (!dayHeld.meta.changes) {
    await env.DB.prepare("UPDATE usage SET bytes = MAX(0, bytes - ?) WHERE id = 1").bind(size).run();
    return refuse("Today's sharing limit has been reached. Try again tomorrow", 507);
  }

  try {
    const data = new Uint8Array(await req.arrayBuffer());
    if (data.length !== size || data.length > MAX_CLIP_BYTES || !isMp4(data)) throw new RangeError("not an MP4");
    const id = randomId();
    const token = randomToken();
    await env.CLIPS.put(`${id}.mp4`, data, { httpMetadata: { contentType: "video/mp4" } });
    const now = Date.now();
    const expires = now + KEEP_DAYS * DAY_MS;
    await env.DB.prepare("INSERT INTO clips (id, created_at, expires_at, size, title, game, duration, width, height, delete_hash) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)")
      .bind(
        id, now, expires, data.length,
        headerText(req.headers.get("x-title"), 100) || "Clip",
        headerText(req.headers.get("x-game"), 60),
        number(req.headers.get("x-duration"), 0, 3600, 0),
        number(req.headers.get("x-width"), 16, 7680, 1280),
        number(req.headers.get("x-height"), 16, 7680, 720),
        await sha256(token),
      )
      .run();
    return json({ id, url: `https://clips.hindsighthq.app/c/${id}`, delete_token: token, expires_at: expires }, 201);
  } catch (e) {
    await release(env, size);
    if (e instanceof RangeError) return refuse("Only MP4 clips can be shared", 415);
    throw e;
  }
}

async function ownedClip(req, env, id) {
  const token = (req.headers.get("authorization") || "").replace(/^Bearer /, "");
  if (!validId(id) || !token) return null;
  const clip = await env.DB.prepare("SELECT * FROM clips WHERE id = ?").bind(id).first();
  return clip && clip.delete_hash === (await sha256(token)) ? clip : null;
}

async function uploadThumb(req, env, id) {
  if (!(await ownedClip(req, env, id))) return refuse("not found", 404);
  const size = Number(req.headers.get("content-length") || 0);
  if (!size || size > MAX_THUMB_BYTES) return refuse("thumbnail too large", 413);
  const data = new Uint8Array(await req.arrayBuffer());
  if (data.length > MAX_THUMB_BYTES || !isJpeg(data)) return refuse("thumbnail must be a JPEG", 415);
  await env.CLIPS.put(`${id}.jpg`, data, { httpMetadata: { contentType: "image/jpeg" } });
  await env.DB.prepare("UPDATE clips SET has_thumb = 1 WHERE id = ?").bind(id).run();
  return json({ ok: true });
}

/** Removes every trace of a clip. Also what expiry and a takedown run. */
export async function removeClip(env, ctx, clip) {
  await env.CLIPS.delete([`${clip.id}.mp4`, `${clip.id}.jpg`]);
  await env.DB.batch([
    env.DB.prepare("DELETE FROM clips WHERE id = ?").bind(clip.id),
    env.DB.prepare("DELETE FROM reactions WHERE clip_id = ?").bind(clip.id),
    env.DB.prepare("UPDATE usage SET bytes = MAX(0, bytes - ?) WHERE id = 1").bind(clip.size),
  ]);
}

async function unshare(req, env, ctx, id) {
  const clip = await ownedClip(req, env, id);
  if (!clip) return refuse("not found", 404);
  await removeClip(env, ctx, clip);
  return json({ ok: true });
}

/* ---------- reactions and reports ---------- */

async function reactionCounts(env, id, voter) {
  const { results } = await env.DB.prepare("SELECT emoji, COUNT(*) AS n FROM reactions WHERE clip_id = ? GROUP BY emoji").bind(id).all();
  const mine = voter ? await env.DB.prepare("SELECT emoji FROM reactions WHERE clip_id = ? AND voter = ?").bind(id, voter).first() : null;
  return { counts: Object.fromEntries(results.map((r) => [r.emoji, r.n])), mine: mine?.emoji ?? null };
}

const voterId = (env, id, value) => (/^[a-f0-9]{32}$/.test(value ?? "") ? sha256(`${env.HASH_SALT}|${id}|${value}`) : null);

async function react(req, env, id) {
  if (!(await visibleClip(env, id))) return refuse("not found", 404);
  if (req.method === "GET") return json(await reactionCounts(env, id, await voterId(env, id, new URL(req.url).searchParams.get("voter"))));
  const { emoji, voter: raw } = await req.json().catch(() => ({}));
  const voter = await voterId(env, id, raw);
  if (!voter || (emoji !== null && !EMOJI.includes(emoji))) return refuse("invalid reaction", 400);
  if (emoji === null) await env.DB.prepare("DELETE FROM reactions WHERE clip_id = ? AND voter = ?").bind(id, voter).run();
  else {
    await env.DB.prepare("INSERT INTO reactions (clip_id, voter, emoji) VALUES (?1, ?2, ?3) ON CONFLICT (clip_id, voter) DO UPDATE SET emoji = ?3").bind(id, voter, emoji).run();
  }
  return json(await reactionCounts(env, id, voter));
}

/** Emails the owner about a report, so nobody has to watch the moderation page. */
async function alertOwner(env, id, reason, detail, total, hidden) {
  if (!env.EMAIL || !env.ALERT_TO) return;
  const clip = await env.DB.prepare("SELECT title, game FROM clips WHERE id = ?").bind(id).first();
  const lines = [
    `Clip: ${clip ? `${clip.title} (${clip.game || "no game"})` : "already gone"}`,
    `Link: https://clips.hindsighthq.app/c/${id}`,
    `Reason: ${reason}`,
    `Details: ${detail || "none"}`,
    `Open reports for this clip: ${total}${hidden ? " (now hidden until you review it)" : ""}`,
    "",
    "Review it at https://reports.hindsighthq.app/clips",
  ];
  try {
    await env.EMAIL.send({ to: env.ALERT_TO, from: { email: "reports@hindsighthq.app", name: "Hindsight clips" }, subject: `Clip reported: ${reason}${hidden ? " (hidden)" : ""}`, text: lines.join("\n") });
  } catch (e) {
    console.error("report alert", e.message);
  }
}

async function report(req, env, id) {
  if (!validId(id)) return refuse("not found", 404);
  const body = await req.json().catch(() => ({}));
  const reason = REPORT_REASONS.includes(body.reason) ? body.reason : null;
  if (!reason) return refuse("pick a reason", 400);
  const reporter = await sha256(`${env.HASH_SALT}|report|${ipOf(req)}`);
  const detail = cleanText(body.detail, 1000);
  const saved = await env.DB.prepare("INSERT OR IGNORE INTO reports (clip_id, created_at, reason, detail, contact, reporter) VALUES (?, ?, ?, ?, ?, ?)")
    .bind(id, Date.now(), reason, detail, cleanText(body.contact, 200), reporter)
    .run();
  const { n } = await env.DB.prepare("SELECT COUNT(*) AS n FROM reports WHERE clip_id = ? AND resolved = 0").bind(id).first();
  const hidden = n >= HIDE_AFTER_REPORTS;
  if (hidden) await env.DB.prepare("UPDATE clips SET hidden = 1 WHERE id = ?").bind(id).run();
  if (saved.meta.changes) await alertOwner(env, id, reason, detail, n, hidden);
  return json({ ok: true }, 201);
}

/* ---------- routing ---------- */

async function route(req, env, ctx, url) {
  const path = url.pathname;
  if (path === "/") return Response.redirect("https://hindsighthq.app/", 302);
  let m;
  if (req.method === "GET" && (m = path.match(/^\/c\/([^/]+)$/))) return sharePage(env, url, m[1]);
  if (["GET", "HEAD"].includes(req.method) && (m = path.match(/^\/v\/([^/]+)\.mp4$/))) return serveFile(req, env, ctx, url, m[1], "mp4", "video/mp4");
  if (["GET", "HEAD"].includes(req.method) && (m = path.match(/^\/t\/([^/]+)\.jpg$/))) return serveFile(req, env, ctx, url, m[1], "jpg", "image/jpeg");

  if (path.startsWith("/api/") && req.method !== "GET") {
    const { success } = await env.WRITE_LIMIT.limit({ key: ipOf(req) });
    if (!success) return refuse("too many requests, try again in a minute", 429);
  }
  if (req.method === "POST" && path === "/api/clips") return upload(req, env);
  if ((m = path.match(/^\/api\/clips\/([^/]+)\/thumb$/)) && req.method === "PUT") return uploadThumb(req, env, m[1]);
  if ((m = path.match(/^\/api\/clips\/([^/]+)$/)) && req.method === "DELETE") return unshare(req, env, ctx, m[1]);
  if ((m = path.match(/^\/api\/clips\/([^/]+)\/reactions$/)) && ["GET", "POST"].includes(req.method)) return react(req, env, m[1]);
  if ((m = path.match(/^\/api\/clips\/([^/]+)\/report$/)) && req.method === "POST") return report(req, env, m[1]);
  return refuse("not found", 404);
}

async function cleanUp(env, ctx) {
  const { results } = await env.DB.prepare("SELECT * FROM clips WHERE expires_at <= ? LIMIT 200").bind(Date.now()).all();
  for (const clip of results) await removeClip(env, ctx, clip);
  const yesterday = new Date(Date.now() - DAY_MS).toISOString().slice(0, 10);
  await env.DB.batch([
    env.DB.prepare("UPDATE usage SET bytes = (SELECT COALESCE(SUM(size), 0) FROM clips) WHERE id = 1"),
    env.DB.prepare("DELETE FROM uploader_days WHERE day < ?").bind(yesterday),
    env.DB.prepare("DELETE FROM upload_days WHERE day < ?").bind(yesterday),
    // The privacy policy promises reports are gone after twelve months.
    env.DB.prepare("DELETE FROM reports WHERE created_at < ?").bind(Date.now() - 365 * DAY_MS),
  ]);
}

export default {
  async fetch(req, env, ctx) {
    const url = new URL(req.url);
    try {
      if (await stopped(env)) return new Response("Hindsight clips are resting until next month.", { status: 503, headers: { "retry-after": "86400" } });
      count(env, ctx);
      return await route(req, env, ctx, url);
    } catch (e) {
      console.error(e);
      return refuse("server error", 500);
    }
  },
  async scheduled(_event, env, ctx) {
    ctx.waitUntil(cleanUp(env, ctx));
  },
};
