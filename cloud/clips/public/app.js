// The share page: copy link, reactions and the report dialog.
const article = document.querySelector("[data-clip]");
const id = article.dataset.clip;

function voterId() {
  let v = "";
  try {
    v = localStorage.getItem("voter") || "";
  } catch {}
  if (!/^[a-f0-9]{32}$/.test(v)) {
    v = [...crypto.getRandomValues(new Uint8Array(16))].map((b) => b.toString(16).padStart(2, "0")).join("");
    try {
      localStorage.setItem("voter", v);
    } catch {}
  }
  return v;
}

const buttons = [...document.querySelectorAll("#reactions button")];
function draw({ counts, mine }) {
  for (const b of buttons) {
    const n = counts[b.dataset.emoji] || 0;
    b.querySelector("b").textContent = n > 999 ? `${Math.floor(n / 1000)}k` : n;
    b.classList.toggle("zero", !n);
    b.setAttribute("aria-pressed", String(mine === b.dataset.emoji));
  }
}

const api = `/api/clips/${id}`;
fetch(`${api}/reactions?voter=${voterId()}`).then((r) => (r.ok ? r.json() : null)).then((d) => d && draw(d)).catch(() => {});

for (const b of buttons) {
  b.addEventListener("click", async () => {
    const on = b.getAttribute("aria-pressed") === "true";
    const r = await fetch(`${api}/reactions`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ emoji: on ? null : b.dataset.emoji, voter: voterId() }),
    }).catch(() => null);
    if (r?.ok) draw(await r.json());
  });
}

const copy = document.getElementById("copy");
copy.addEventListener("click", async () => {
  await navigator.clipboard.writeText(location.href).catch(() => {});
  copy.textContent = "Copied";
  setTimeout(() => (copy.textContent = "Copy link"), 1600);
});

const dialog = document.getElementById("report-dialog");
const form = document.getElementById("report");
const result = document.getElementById("report-result");
const send = document.getElementById("report-send");
document.getElementById("flag").addEventListener("click", () => {
  result.textContent = "";
  dialog.showModal();
});
document.getElementById("report-cancel").addEventListener("click", () => dialog.close());
// A click on the backdrop closes it, like Escape.
dialog.addEventListener("click", (e) => e.target === dialog && dialog.close());

form.addEventListener("submit", async (e) => {
  e.preventDefault();
  send.disabled = true;
  const data = Object.fromEntries(new FormData(form));
  const r = await fetch(`${api}/report`, { method: "POST", headers: { "content-type": "application/json" }, body: JSON.stringify(data) }).catch(() => null);
  if (r?.ok) {
    result.textContent = "Thanks. It will be reviewed.";
    form.reset();
    setTimeout(() => dialog.close(), 1400);
  } else {
    result.textContent = "Couldn't send that. Try again.";
  }
  send.disabled = false;
});
