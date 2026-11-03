// Hindsight backend.
//   api.hindsighthq.app      GET /update/latest.json, GET /update/<file>, POST /report
//   reports.hindsighthq.app  private report viewer and clip moderation (behind Cloudflare Access)
//
// Update files live in R2 and are signed; the app verifies the signature, so
// this server is not trusted with the code it hands out. Reports are stored
// without IP addresses. Their contents are user input: everything shown in the
// viewer is escaped, and the viewer page forbids scripts.

const MAX_REPORT_BYTES = 2_000_000;
const SAFE_FILE = /^[A-Za-z0-9._-]+$/;

const json = (body, status = 200) =>
  new Response(JSON.stringify(body), { status, headers: { "content-type": "application/json" } });

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    try {
      if (url.hostname.startsWith("reports.")) return await viewer(req, env, url);
      if (req.method === "GET" && url.pathname.startsWith("/update/")) return await update(env, url);
      if (req.method === "POST" && url.pathname === "/report") return await report(req, env);
      return json({ error: "not found" }, 404);
    } catch (e) {
      console.error(e);
      return json({ error: "server error" }, 500);
    }
  },
  // The privacy policy promises problem reports are gone after twelve months.
  async scheduled(_event, env, ctx) {
    ctx.waitUntil(
      (async () => {
        const cutoff = Date.now() - 365 * 86_400_000;
        const { results } = await env.DB.prepare("SELECT id FROM reports WHERE created_at < ? LIMIT 500").bind(cutoff).all();
        if (!results.length) return;
        await env.REPORTS.delete(results.map((r) => `${r.id}.json`));
        await env.DB.prepare("DELETE FROM reports WHERE created_at < ?").bind(cutoff).run();
      })(),
    );
  },
};

/* ---------- updates ---------- */

async function update(env, url) {
  const name = url.pathname.slice("/update/".length);
  // The test channel lets a build be tried end to end before anyone is offered it.
  const key = (url.searchParams.get("channel") === "test" ? "test/" : "") + name;
  if (!SAFE_FILE.test(name)) return json({ error: "not found" }, 404);
  const object = await env.UPDATES.get(key);
  if (!object) return json({ error: "not found" }, 404);
  const headers = new Headers();
  object.writeHttpMetadata(headers);
  headers.set("etag", object.httpEtag);
  headers.set("content-length", object.size);
  headers.set("cache-control", name === "latest.json" ? "no-cache" : "public, max-age=86400");
  return new Response(object.body, { headers });
}

/* ---------- reports ---------- */

async function report(req, env) {
  const ip = req.headers.get("cf-connecting-ip") || "unknown";
  const { success } = await env.REPORT_LIMIT.limit({ key: ip });
  if (!success) return json({ error: "too many reports, try again in a minute" }, 429);

  const length = Number(req.headers.get("content-length") || 0);
  if (!length || length > MAX_REPORT_BYTES) return json({ error: "report too large" }, 413);
  const text = await req.text();
  if (text.length > MAX_REPORT_BYTES) return json({ error: "report too large" }, 413);

  let r;
  try {
    r = JSON.parse(text);
  } catch {
    return json({ error: "invalid report" }, 400);
  }
  const description = typeof r.description === "string" ? r.description.trim() : "";
  if (!description || description.length > 10_000) return json({ error: "a description is required" }, 400);

  const str = (v, max) => (typeof v === "string" ? v.slice(0, max) : "");
  const id = crypto.randomUUID().replaceAll("-", "").slice(0, 10).toUpperCase();
  await env.REPORTS.put(`${id}.json`, text, { httpMetadata: { contentType: "application/json" } });
  await env.DB.prepare("INSERT INTO reports (id, created_at, version, os, gpu, summary, contact) VALUES (?, ?, ?, ?, ?, ?, ?)")
    .bind(id, Date.now(), str(r.app_version, 40), str(r.system?.os, 120), str(r.system?.gpu, 200), description.slice(0, 200), str(r.contact, 200))
    .run();
  return json({ id }, 201);
}

/* ---------- viewer (Cloudflare Access + JWT check) ---------- */

let jwks = null;

const b64url = (s) => Uint8Array.from(atob(s.replaceAll("-", "+").replaceAll("_", "/").padEnd(Math.ceil(s.length / 4) * 4, "=")), (c) => c.charCodeAt(0));

// Access guards the hostname; checking its token as well means a routing or
// configuration mistake can't expose reports.
async function accessOk(req, env) {
  const token = req.headers.get("cf-access-jwt-assertion");
  if (!token) return false;
  const [h, p, sig] = token.split(".");
  if (!sig) return false;
  const dec = new TextDecoder();
  const header = JSON.parse(dec.decode(b64url(h)));
  const claims = JSON.parse(dec.decode(b64url(p)));
  const iss = `https://${env.ACCESS_TEAM}.cloudflareaccess.com`;
  const aud = Array.isArray(claims.aud) ? claims.aud : [claims.aud];
  if (claims.iss !== iss || !aud.includes(env.ACCESS_AUD) || claims.exp * 1000 < Date.now()) return false;

  const verify = async () => {
    const jwk = jwks?.find((k) => k.kid === header.kid);
    if (!jwk) return null;
    const key = await crypto.subtle.importKey("jwk", jwk, { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" }, false, ["verify"]);
    return crypto.subtle.verify("RSASSA-PKCS1-v1_5", key, b64url(sig), new TextEncoder().encode(`${h}.${p}`));
  };
  let ok = await verify();
  if (ok === null) {
    jwks = (await (await fetch(`${iss}/cdn-cgi/access/certs`)).json()).keys;
    ok = await verify();
  }
  return ok === true;
}

const esc = (v) =>
  String(v ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);

const page = (title, body) =>
  new Response(
    `<!doctype html><meta charset="utf-8"><title>${esc(title)}</title>
<style>
body{background:#0f1012;color:#e7e8eb;font:14px/1.5 system-ui,sans-serif;margin:0 auto;max-width:1100px;padding:24px}
a{color:#ee5d62}table{border-collapse:collapse;width:100%}td,th{border-bottom:1px solid #2d3037;padding:8px;text-align:left;vertical-align:top}
th{color:#a2a6af;font-weight:600}pre{background:#18191d;border:1px solid #2d3037;border-radius:6px;overflow:auto;padding:12px;white-space:pre-wrap;word-break:break-word}
h1{font-size:20px}h2{font-size:15px;margin-top:28px}.m{color:#a2a6af}
</style>${body}`,
    { headers: { "content-type": "text/html; charset=utf-8", "content-security-policy": "default-src 'none'; style-src 'unsafe-inline'; media-src 'self'; form-action 'self'", "x-content-type-options": "nosniff" } },
  );

/* Shared clips that were reported. Takedowns reach every cache within ten
 * minutes, the time the clips Worker lets a file stay cached. */
async function moderation(req, env, url, id, action) {
  if (req.method === "POST") {
    if (!id || action === ".mp4" || req.headers.get("origin") !== url.origin) return new Response("Forbidden", { status: 403 });
    if (action === "/remove") {
      const row = await env.CLIPS_DB.prepare("SELECT size FROM clips WHERE id = ?").bind(id).first();
      await env.CLIPS.delete([`${id}.mp4`, `${id}.jpg`]);
      await env.CLIPS_DB.batch([
        env.CLIPS_DB.prepare("DELETE FROM clips WHERE id = ?").bind(id),
        env.CLIPS_DB.prepare("DELETE FROM reactions WHERE clip_id = ?").bind(id),
        env.CLIPS_DB.prepare("UPDATE reports SET resolved = 1 WHERE clip_id = ?").bind(id),
        env.CLIPS_DB.prepare("UPDATE usage SET bytes = MAX(0, bytes - ?) WHERE id = 1").bind(row?.size ?? 0),
      ]);
    } else {
      await env.CLIPS_DB.batch([
        env.CLIPS_DB.prepare("UPDATE clips SET hidden = 0 WHERE id = ?").bind(id),
        env.CLIPS_DB.prepare("UPDATE reports SET resolved = 1 WHERE clip_id = ?").bind(id),
      ]);
    }
    return Response.redirect(`${url.origin}/clips`, 303);
  }
  if (req.method !== "GET") return new Response("Method not allowed", { status: 405 });
  if (action === ".mp4") {
    const object = await env.CLIPS.get(`${id}.mp4`);
    if (!object) return new Response("Not found", { status: 404 });
    return new Response(object.body, { headers: { "content-type": "video/mp4", "x-content-type-options": "nosniff" } });
  }
  const { results } = await env.CLIPS_DB.prepare(
    `SELECT r.clip_id, r.created_at, r.reason, r.detail, r.contact, c.title, c.game, c.hidden
     FROM reports r LEFT JOIN clips c ON c.id = r.clip_id WHERE r.resolved = 0 ORDER BY r.created_at DESC LIMIT 200`,
  ).all();
  const byClip = new Map();
  for (const r of results) byClip.set(r.clip_id, [...(byClip.get(r.clip_id) ?? []), r]);
  const usage = await env.CLIPS_DB.prepare("SELECT bytes, (SELECT COUNT(*) FROM clips) AS clips FROM usage").first();
  const blocks = [...byClip].map(([cid, reports]) => {
    const c = reports[0];
    const lines = reports.map((r) => `<li>${esc(r.reason)}: ${esc(r.detail || "no details")} <span class="m">${esc(r.contact)}</span></li>`).join("");
    if (c.title == null) return `<h2>${esc(cid)} (already gone)</h2><ul>${lines}</ul>`;
    return `<h2>${esc(c.title)} <span class="m">${esc(c.game)}${c.hidden ? " · hidden" : ""}</span></h2>
<video src="/clips/${esc(cid)}.mp4" controls width="480"></video><ul>${lines}</ul>
<form method="post" action="/clips/${esc(cid)}/remove"><button>Remove clip</button></form>
<form method="post" action="/clips/${esc(cid)}/keep"><button>Keep clip</button></form>`;
  });
  return page("Reported clips", `<p><a href="/">All reports</a></p><h1>Reported clips</h1>
<p class="m">${usage.clips} clips stored, ${(usage.bytes / 1e9).toFixed(1)} of 500 GB.</p>${blocks.join("") || '<p class="m">Nothing reported.</p>'}`);
}

async function viewer(req, env, url) {
  if (!(await accessOk(req, env))) return new Response("Forbidden", { status: 403 });
  const clip = url.pathname.match(/^\/clips(?:\/([2-9a-zA-Z]{10})(\.mp4|\/remove|\/keep))?$/);
  if (clip) return await moderation(req, env, url, clip[1], clip[2]);
  if (req.method !== "GET") return new Response("Method not allowed", { status: 405 });

  const m = url.pathname.match(/^\/r\/([A-Z0-9]+)(\.json)?$/);
  if (m) {
    const object = await env.REPORTS.get(`${m[1]}.json`);
    if (!object) return new Response("Not found", { status: 404 });
    if (m[2]) return new Response(object.body, { headers: { "content-type": "application/json" } });
    const r = await object.json();
    const { logs = {}, crashes = "", description, contact, ...rest } = r;
    const logBlocks = Object.entries(logs).map(([k, v]) => `<h2>${esc(k)}</h2><pre>${esc(v)}</pre>`).join("");
    return page(`Report ${m[1]}`, `<p><a href="/">All reports</a> · <a href="/r/${m[1]}.json">Raw JSON</a></p>
<h1>Report ${esc(m[1])}</h1><p class="m">Contact: ${esc(contact || "none")}</p>
<h2>What happened</h2><pre>${esc(description)}</pre>
<h2>System and settings</h2><pre>${esc(JSON.stringify(rest, null, 2))}</pre>
<h2>Crash events</h2><pre>${esc(crashes || "none")}</pre>${logBlocks}`);
  }

  const { results } = await env.DB.prepare("SELECT * FROM reports ORDER BY created_at DESC LIMIT 200").all();
  const rows = results
    .map((r) => `<tr><td><a href="/r/${esc(r.id)}">${esc(r.id)}</a></td><td>${esc(new Date(r.created_at).toISOString().slice(0, 16).replace("T", " "))}</td><td>${esc(r.version)}</td><td>${esc(r.os)}<br><span class="m">${esc(r.gpu)}</span></td><td>${esc(r.summary)}</td></tr>`)
    .join("");
  return page("Hindsight reports", `<h1>Hindsight reports</h1><p><a href="/clips">Reported clips</a></p><table><tr><th>ID</th><th>Received (UTC)</th><th>Version</th><th>System</th><th>Summary</th></tr>${rows || '<tr><td colspan="5" class="m">No reports yet.</td></tr>'}</table>`);
}
